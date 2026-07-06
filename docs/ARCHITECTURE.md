# insane-cli — Arquitetura

## Visão geral

`insane-cli` é um CLI Rust, binário único e multiplataforma (Windows/Linux/macOS), que consome a API NVIDIA NIM (OpenAI-compatível) para tarefas de assistência à programação. O desenho prioriza, nesta ordem: **segurança → respeito ao rate limit → correção → desempenho → UX**.

## Decisões principais

| Decisão | Escolha | Justificativa |
|---|---|---|
| Linguagem | Rust 1.96 | Binário único sem runtime, inicialização < 50 ms, baixo consumo de memória, concorrência segura por tipos (`Send`/`Sync`), cancelamento cooperativo confiável. Go não estava disponível no ambiente; Node/Python têm startup e memória piores para CLI. |
| HTTP | `reqwest` + rustls | Pool de conexões e keep-alive reutilizados num único `Client`; rustls evita dependência de OpenSSL nativo no Windows. |
| Async | `tokio` | Necessário para streaming com backpressure, fila do rate limiter e Ctrl+C imediato. |
| CLI | `clap` (derive) | Padrão de fato; parsing sem custo de startup relevante. |
| Chave de API | env `NVIDIA_API_KEY` ou keyring do SO | Nunca em arquivo de config, logs ou código. Redaction global de `nvapi-…` em qualquer saída de erro/log. |
| Rate limit | Sliding-window log, 40/60 s | Janela móvel exata (log de timestamps) em vez de token bucket, porque o requisito é "nenhuma janela móvel de 60 s excede 40" — o log garante isso por construção; o bucket permitiria rajadas na fronteira. |

## Componentes

```
┌──────────┐   ┌───────────┐   ┌────────────────┐   ┌───────────────┐
│ cli.rs   │──▶│ commands/ │──▶│ client/nim.rs  │──▶│ NVIDIA NIM API │
│ (clap)   │   │ (10 cmds) │   │  retry+backoff │   └───────────────┘
└──────────┘   └─────┬─────┘   └───────┬────────┘
                     │                 │ toda req passa por
      ┌──────────────┼──────────┐      ▼
      ▼              ▼          ▼   ┌────────────┐
┌──────────┐  ┌───────────┐ ┌─────┐│ limiter.rs │  fila única FIFO
│context.rs│  │fileops.rs │ │cache││ 40/60s     │  compartilhada (Arc)
│ leitura  │  │ diff+atô- │ └─────┘└────────────┘
│ seletiva │  │ mico+roll-│
│+secrets  │  │ back      │
└──────────┘  └───────────┘
```

- **`cli.rs`** — definição declarativa de comandos/flags; nenhuma lógica.
- **`config.rs`** — resolve configuração com precedência `flags > env > arquivo TOML > padrão`. `rate_limit.rpm` tem teto 40 quando a `base_url` é o endpoint público da NIM.
- **`client/`** — trait `LlmClient` isola o provedor: trocar modelo, endpoint ou provedor exige só outra implementação da trait, sem tocar comandos. `nim.rs` faz retry com backoff exponencial + jitter apenas para erros transitórios (429/5xx/rede); `Retry-After` tem precedência e penaliza o limiter globalmente. `sse.rs` é um parser SSE incremental: processa bytes conforme chegam, tolera chunks inválidos e respeita cancelamento — streaming com backpressure natural (o `Stream` só é puxado quando o consumidor imprime).
- **`limiter.rs`** — único ponto de saída de requisições. `acquire().await` FIFO; `capacity`/`window` parametrizáveis para testes (produção fixa 40/60 s). Métricas expostas em `status` sem dados sensíveis.
- **`context.rs`** — monta o contexto por leitura seletiva (arquivos citados, `--lines A:B`, limite `max_context_bytes`), nunca o projeto inteiro; respeita `.gitignore`, lista própria do CLI e lista fixa de arquivos sensíveis.
- **`secrets.rs`** — regexes para chaves AWS, `ghp_`, `nvapi-`, PEM, JWT, `password=`; bloqueia envio sem confirmação e redige logs.
- **`fileops.rs`** — mudanças só com diff exibido + confirmação explícita; escrita via temp-file + rename atômico no mesmo volume; backup `.insane-bak` permite rollback.
- **`cache.rs`** — opcional, off por padrão; chave `sha256(base_url+model+messages+params)`, TTL configurável, invalidável por comando.
- **`output.rs`** — resposta do modelo em **stdout**; logs técnicos (`tracing`) em **stderr**; `--json` emite objeto único com resposta, usage, timing e métricas do limiter.

## Fluxo de uma requisição

1. Comando monta `ChatRequest` (contexto já filtrado por `secrets.rs`).
2. Cache consultado (se habilitado) — hit evita chamada de rede.
3. `limiter.acquire().await` — bloqueia até haver slot na janela móvel.
4. `reqwest` envia (conexão reutilizada). Timeout configurável.
5. Sucesso: stream SSE impresso incrementalmente ou resposta única. Falha transitória: backoff+jitter e volta ao passo 3 (retry também consome slot). Falha permanente: erro classificado, sem retry.
6. Ctrl+C em qualquer ponto aborta a request em voo e sai com código 130.

## Tratamento de erros e exit codes

Erros são classificados em `Permanent`, `Transient` e `RateLimited{retry_after}`. Exit codes: `0` ok, `1` erro, `2` uso inválido, `3` autenticação, `4` rate limit esgotado, `130` cancelado.

## Testes

Servidor NIM simulado (sem chave real) com contagem timestamped de requisições, modos 429/500, SSE configurável (delay, chunk inválido, corte abrupto). O teste de carga dispara chamadas concorrentes por um limiter de janela reduzida e verifica no lado do *servidor* que nenhuma janela móvel excede a capacidade — a prova é feita onde importa.

## Limitações conhecidas

- Contagem de tokens é aproximada (bytes/heurística), suficiente para trim de contexto.
- O limite de 40 rpm é imposto no cliente; múltiplos processos do CLI não compartilham o limiter (documentado no README).
