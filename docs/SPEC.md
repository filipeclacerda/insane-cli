# insane-cli — Especificação Técnica (ground truth para implementação)

CLI de assistência à programação consumindo a API NVIDIA NIM. Linguagem: **Rust** (edition 2021, toolchain 1.96).
Prioridade em decisões ambíguas: segurança > rate limit > correção > desempenho > UX.

## 1. API NVIDIA NIM (verificada em 2026-07)

- Base URL: `https://integrate.api.nvidia.com/v1` (configurável — permitir trocar endpoint/provedor).
- Autenticação: header `Authorization: Bearer <key>`; chaves têm prefixo `nvapi-`.
- Chat: `POST /chat/completions` — formato OpenAI-compatível:
  - Request: `{ "model": "...", "messages": [{"role":"system|user|assistant","content":"..."}], "temperature", "top_p", "max_tokens", "stream": bool }`
  - Response não-stream: `{ "id", "choices": [{"message": {"role","content"}, "finish_reason"}], "usage": {"prompt_tokens","completion_tokens","total_tokens"} }`
  - Streaming: SSE (`text/event-stream`), linhas `data: {json}`, chunks com `choices[0].delta.content`, terminado por `data: [DONE]`.
- Modelos: `GET /models` → `{ "data": [{"id": "..."}] }`.
- Rate limit: **40 requisições/minuto** (janela móvel). Pode retornar `429` com header `Retry-After` (segundos).
- Erros: 400 (permanente, não retry), 401/403 (chave inválida, não retry), 404 (modelo inexistente, não retry), 429 (retry após espera), 5xx (transitório, retry com backoff), timeouts/erros de rede (retry).
- Modelo padrão: `meta/llama-3.3-70b-instruct` (configurável).

## 2. Crates permitidas (manter enxuto)

- `clap` (derive) — CLI
- `tokio` (rt-multi-thread, macros, signal, sync, time, io-std, fs) — async runtime
- `reqwest` (json, stream, rustls-tls; `default-features = false`) — HTTP com pool de conexões
- `serde`, `serde_json`, `toml` — serialização/config
- `futures-util` — streams
- `tracing`, `tracing-subscriber` (env-filter, json) — logs estruturados (stderr)
- `directories` — paths de config/cache por SO
- `keyring` — armazenamento seguro de chave (Windows Credential Manager / macOS Keychain / secret-service)
- `ignore` — respeitar .gitignore
- `regex` — detecção de segredos
- `similar` — diffs
- `tempfile` — escrita atômica
- `thiserror`, `anyhow` — erros
- `rand` — jitter
- `sha2` — chaves de cache
- dev: `axum` ou `httpmock`/`wiremock` p/ mock NIM; `assert_cmd`, `predicates`, `tempfile`

## 3. Árvore de diretórios

```
insane-cli/
├── Cargo.toml
├── README.md
├── docs/
│   ├── SPEC.md
│   ├── ARCHITECTURE.md
│   ├── BENCHMARKS.md
│   └── REPORT.md
├── examples/config.example.toml
├── src/
│   ├── main.rs            # bootstrap mínimo, ctrl-c, exit codes
│   ├── cli.rs             # clap: comandos e flags globais
│   ├── config.rs          # precedência flags > env > arquivo > padrão
│   ├── error.rs           # ApiError{permanent|transient|rate_limited}, exit codes
│   ├── client/
│   │   ├── mod.rs         # trait LlmClient (permite trocar provedor)
│   │   ├── nim.rs         # implementação NIM/OpenAI-compat, retry+backoff+jitter
│   │   └── sse.rs         # parser SSE incremental (sem carregar corpo inteiro)
│   ├── limiter.rs         # sliding-window 40/60s + fila única FIFO
│   ├── context.rs         # montagem/redução de contexto, leitura seletiva
│   ├── fileops.rs         # escrita atômica, backup/rollback, diff, confirmação
│   ├── secrets.rs         # detecção de segredos + redaction em logs
│   ├── cache.rs           # cache opcional em disco (sha256(model+messages)), invalidável
│   ├── output.rs          # texto/JSON, quiet, separação stdout(resposta)/stderr(logs)
│   ├── session.rs         # chat interativo (histórico em memória, trim por tokens aprox.)
│   └── commands/
│       ├── mod.rs
│       ├── ask.rs  chat.rs  explain.rs  review.rs  fix.rs
│       ├── refactor.rs  test.rs  config_cmd.rs  models.rs  status.rs
├── tests/
│   ├── mock_nim.rs        # servidor NIM simulado (helper comum)
│   ├── rate_limit.rs      # prova: nenhuma janela de 60s excede 40 reqs; concorrência
│   ├── retry.rs           # 429+Retry-After, backoff c/ jitter, erros permanentes sem retry
│   ├── streaming.rs       # SSE, respostas parciais/ inválidas, cancelamento
│   ├── fileops.rs         # atomicidade, rollback
│   ├── secrets.rs         # redaction e detecção
│   └── cli_e2e.rs         # assert_cmd fim-a-fim contra mock
└── benches/ ou scripts de benchmark reproduzíveis
```

## 4. Contratos principais

```rust
// client/mod.rs
#[async_trait-like via Box<dyn ...> ou enum] // preferir trait com async fn (Rust 1.75+)
pub trait LlmClient: Send + Sync {
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, ApiError>;
    async fn chat_stream(&self, req: ChatRequest) -> Result<impl Stream<Item = Result<StreamChunk, ApiError>>, ApiError>;
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ApiError>;
}
```

- **Toda** requisição passa por `RateLimiter::acquire().await` ANTES de sair — inclusive retries e chamadas concorrentes. Um único limiter compartilhado (`Arc`).
- Rate limiter: sliding-window log (`VecDeque<Instant>` sob `tokio::Mutex`), capacidade 40/60s, FIFO justo (usar `tokio::sync::Semaphore`+fila ou mutex garante ordem). `Retry-After` alimenta o limiter (pausa global). Métricas: `used`, `remaining`, `next_slot_in`, `total_waited` — expostas em `status` e `--json`, sem dados sensíveis.
- Backoff: exponencial base 500ms, fator 2, máx 30s, jitter uniforme ±50%, máx 5 tentativas, apenas para 429/5xx/rede. `Retry-After` tem precedência.
- Cancelamento: `tokio::signal::ctrl_c` → `CancellationToken`-like (usar `tokio::select!`); aborta request em voo e sai com código 130.

## 5. Configuração

Precedência: **flags > env > arquivo > padrão**.
- Arquivo: `{config_dir}/insane-cli/config.toml` (via `directories`). Campos: `model`, `base_url`, `timeout_secs`, `max_tokens`, `temperature`, `stream`, `cache.enabled`, `cache.ttl_secs`, `rate_limit.rpm` (default 40, teto 40 para o endpoint NIM público — nunca aceitar valor acima sem `base_url` customizada), `ignore` (lista de globs extra).
- Env: `NVIDIA_API_KEY` (chave — NUNCA logada/gravada em config), `INSANE_MODEL`, `INSANE_BASE_URL`, `INSANE_TIMEOUT`, etc.
- Chave: 1º `NVIDIA_API_KEY`; 2º keyring do SO (`insane-cli`/`nvidia_api_key`). `config set-key` grava no keyring (lendo de stdin sem eco); `config` NUNCA grava chave em arquivo. Redaction: qualquer string casando `nvapi-[A-Za-z0-9_-]+` é substituída por `nvapi-***` em logs/erros/panics.

## 6. Comandos

Flags globais: `--model`, `--json`, `--stream/--no-stream`, `--timeout`, `--quiet`, `--verbose`, `--config <path>`, `--no-cache`, `--yes` (auto-confirmar apenas onde seguro — nunca para escrita de arquivos com segredos detectados).

- `ask <prompt|- (stdin)> [-f arquivo]...` — pergunta única; streaming por padrão em TTY.
- `chat` — sessão interativa (rustyline não; usar stdin simples async p/ manter leveza), `/exit`, `/clear`, `/model <m>`.
- `explain <arquivo|-> [--lines A:B]` — explica código.
- `review <arquivo...|--diff>` — revisão; `--diff` lê `git diff` (ou stdin).
- `fix <arquivo> [--apply]` — propõe correção; mostra diff; `--apply` pede confirmação, escreve atômico com backup `.insane-bak`, `--rollback` restaura.
- `refactor <arquivo> --goal "..." [--apply]` — idem fix.
- `test <arquivo> [-o saída]` — gera testes; escrita só com confirmação.
- `config [get|set|list|path|set-key|unset-key]`.
- `models [--refresh]` — lista modelos (cacheável).
- `status` — verifica API (GET /models), mostra métricas do limiter, config efetiva (sem chave).

Entrada: argumentos, `-` para stdin, `-f/--file` para arquivos. Saída: resposta do modelo em **stdout**; logs em **stderr**. `--json` produz objeto único `{response, model, usage, timing, rate_limiter}`.

## 7. Segurança de arquivos e contexto

- Leitura seletiva: nunca carregar projeto inteiro; ler arquivos citados, limitar por `max_context_bytes` (default 192KiB), truncar com aviso; suporte a `--lines A:B`.
- Respeitar `.gitignore` (crate `ignore`) + lista própria (config `ignore`) + lista fixa para material de chave/certificado: `*.pem`, `*.key`, `id_rsa*`, `*.pfx`, `credentials*`, `secrets*`. Arquivos `.env*` são permitidos.
- Leituras não pedem confirmação extra por conteúdo parecido com segredo; a redação de segredos continua aplicada a logs/erros.
- Escrita: sempre mostrar diff (`similar`) e pedir confirmação; escrita via arquivo temporário no mesmo diretório + rename atômico; backup prévio para rollback.

## 8. Cache

Opcional (off por default). Disco: `{cache_dir}/insane-cli/`. Chave: sha256(base_url+model+messages+params). TTL configurável. `config cache-clear` invalida. Nunca cachear quando streaming interativo de chat, apenas comandos determinísticos (`explain`, `review`, `ask` com `--cache`).

## 9. Desempenho

- Startup: lazy-init do runtime; não tocar rede/keyring a menos que necessário; alvo < 50ms para `--help`.
- Reutilizar `reqwest::Client` único (pool + keep-alive + HTTP/2).
- Streaming incremental com backpressure (imprimir chunk a chunk; `Stream` puxado sob demanda).
- Perfil release: `lto = "thin"`, `codegen-units = 1`, `strip = true`, `panic = "abort"` (cuidado: panic=abort quebra testes — aplicar só no perfil release).

## 10. Testes (mock NIM, sem chave real)

Mock server (axum) com: contagem de requisições timestamped, modo 429+Retry-After, modo 500 transitório, SSE configurável (chunks, delay, chunk inválido no meio, corte abrupto), latência injetável. Provar via teste de carga: disparar 100 requisições concorrentes com limiter em janela reduzida via injeção de relógio/parâmetros (limiter deve aceitar `window` e `capacity` parametrizáveis para teste; produção fixa 40/60s) e verificar que o mock nunca observa >N em qualquer janela móvel. Exit codes: 0 ok, 1 erro genérico, 2 uso inválido, 3 auth, 4 rate limit esgotado, 130 cancelado.
