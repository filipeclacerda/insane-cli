# insane-cli

CLI de assistência à programação em Rust para providers OpenAI-compatible,
com presets para NVIDIA NIM e LM Studio local. Binário único,
multiplataforma (Windows/Linux/macOS), sem runtime gerenciado.

Prioridades de projeto, nesta ordem: **segurança → rate limit → correção →
desempenho → UX**. Veja `docs/SPEC.md` e `docs/ARCHITECTURE.md` para os
detalhes de design, e `docs/REPORT.md` para decisões e desvios conhecidos.

## Instalação

Requer Rust/Cargo 1.96+ (toolchain `stable`).

```bash
git clone <repo>
cd insane-cli
cargo build --release
```

O binário fica em `target/release/insane` (`insane.exe` no Windows).

Para instalar no `PATH` do usuário (via `~/.cargo/bin`):

```bash
cargo install --path .
```

## Configuração da chave de API

**Nunca** coloque a chave em um arquivo de configuração. Duas formas
suportadas, em ordem de precedência:

1. Variável de ambiente `NVIDIA_API_KEY`:
   ```bash
   export NVIDIA_API_KEY=nvapi-...        # bash/zsh
   $env:NVIDIA_API_KEY = "nvapi-..."      # PowerShell
   ```
2. OS keyring (Windows Credential Manager / macOS Keychain / secret-service
   no Linux), via:
   ```bash
   insane config set-key
   ```
   Lê a chave de `stdin` sem eco (prompt digitado, não exposto na tela) e
   grava no keyring do SO. Para remover:
   ```bash
   insane config unset-key
   ```

`insane status` e `insane config list` nunca exibem a chave.

## Uso: os 10 comandos

```bash
# Pergunta única (streaming por padrão em terminal)
insane ask "explique ownership em Rust"

# Lendo o prompt de stdin
echo "resuma isto" | insane ask -

# Com arquivo(s) de contexto (ignore + denylist de chaves/certificados)
insane ask "onde está o bug?" -f src/main.rs

# Sessão de chat interativa (histórico em memória, trim automático)
insane chat
# sem nenhum argumento, `insane` sozinho já roda `chat` (com tools)
insane
# dentro do chat: /exit  /clear  /model <nome>  /tools  /cwd  /continue  /resume
# retomar a última sessão salva para o provider ativo (modelo + histórico)
insane chat --continue

# Explicar um arquivo (ou um intervalo de linhas)
insane explain src/limiter.rs
insane explain src/limiter.rs --lines 10:42
insane explain - < trecho.rs   # via stdin

# Revisar arquivo(s) ou um diff
insane review src/config.rs src/cli.rs
insane review --diff            # roda `git diff` no diretório atual
git diff | insane review --diff -   # diff via stdin

# Propor correção (sempre mostra diff; só grava com --apply + confirmação)
insane fix src/buggy.rs
insane fix src/buggy.rs --apply
insane fix src/buggy.rs --rollback   # restaura o backup .insane-bak

# Refatorar em direção a um objetivo
insane refactor src/legacy.rs --goal "extrair função auxiliar" --apply
insane refactor src/legacy.rs --goal "..." --rollback

# Gerar testes
insane test src/parser.rs
insane test src/parser.rs -o tests/parser_test.rs   # mostra diff/confirma antes de gravar

# Configuração
insane config list                 # config efetiva (sem a chave)
insane config get model
insane config set model "meta/llama-3.3-70b-instruct"
insane config path                 # caminho do config.toml
insane config set-key              # grava chave no keyring (stdin, sem eco)
insane config unset-key
insane config cache-clear          # limpa o cache em disco

# Modelos disponíveis
insane models
insane models --refresh

# Saúde da API + métricas do rate limiter + config efetiva
insane status
```

## Modo agente (chat com tools)

`insane chat` roda com **tool calling habilitado por padrão**: o modelo pode
pedir para listar/ler/pesquisar/escrever/editar arquivos e rodar comandos no
seu diretório atual, em vez de só conversar em texto. Isso transforma o chat
em um agente de código estilo opencode/Claude Code, sempre com confirmação do
usuário antes de qualquer ação perigosa. Prioridades inalteradas: **segurança
→ rate limit → correção → desempenho → UX**. Detalhes de design em
`docs/SPEC-AGENT.md`.

### As 6 tools

| Tool | Parâmetros | Risco | Confirma? |
|---|---|---|---|
| `list_files` | `path?`, `max_entries?` | leitura | Não. Respeita `.gitignore` + `ignore` do config + denylist de chaves/certificados; cap de 500 entradas. |
| `read_file` | `path`, `start_line?`, `end_line?` | leitura → rede | Não pede confirmação extra por conteúdo. Arquivos `.env` são permitidos; arquivos de chave/certificado seguem bloqueados por nome. |
| `search_files` | `pattern`, `path?`, `max_results?` | leitura → rede | Igual a `read_file`, aplicado ao resultado do grep (cap de 100 linhas). |
| `write_file` | `path`, `content` | escrita | **Sempre.** Mostra diff (arquivo novo = diff contra vazio) antes de perguntar. |
| `edit_file` | `path`, `old_string`, `new_string`, `replace_all?` | escrita | **Sempre.** `old_string` precisa ser único no arquivo, a menos que `replace_all` seja passado; mostra diff antes de perguntar. |
| `run_command` | `command`, `timeout_secs?` | execução | **Sempre**, mostrando o comando exato antes de perguntar. |

**Aviso:** `run_command` executa o comando no seu shell (PowerShell 7
`pwsh`, com fallback para Windows PowerShell, no Windows; Bash com fallback
para `sh` no Unix), com as suas permissões de usuário, dentro do
diretório atual. Não há sandbox de processo — só a confirmação explícita
antes de cada execução é o que impede um comando de rodar sem o seu
conhecimento.

Toda tool opera **apenas dentro do diretório de trabalho atual**: caminhos
são canonicalizados e qualquer escape via `..`, caminho absoluto fora do cwd,
ou symlink que aponte para fora, é rejeitado como erro devolvido ao modelo
(nunca ao usuário como uma falha de comando) — o arquivo fora do sandbox
nunca é tocado.

### Fluxo de permissões (y/n/a)

Antes de uma escrita, edição ou execução de comando, o CLI pergunta em stderr:

```
Write to src/main.rs? [y/n/a]
```

- `y` — aprova só esta chamada.
- `n` (ou qualquer outra entrada, ou Enter vazio) — recusa. Sempre a resposta
  padrão em terminal não interativo (`stdin` não é um TTY): nunca
  destrutivo por padrão.
- `a` — aprova esta tool (`write_file` ou `edit_file`)
  **para o resto desta sessão de chat**. Para `run_command`, `a` só lembra o
  **comando exato repetido**, nunca "sempre rodar qualquer comando" — não
  existe um `--yolo` e nenhum bypass global foi implementado.

`--no-tools` (flag global, funciona com ou sem subcomando explícito)
desabilita completamente o modo agente (volta ao chat de texto puro da fase
1/2). `ask --tools` roda **uma rodada não interativa** do loop agêntico para
um único prompt; como `ask` normalmente roda com `stdin` não interativo, toda
escrita/edição/comando pedido pelo modelo é recusada automaticamente
(comportamento correto e esperado, não um bug).

Rodar `insane` **sem nenhum subcomando** resolve para `chat` (com tools, a
menos que `--no-tools` também tenha sido passado) -- é o atalho mais curto
para abrir o modo agente.

Dentro do chat, os slash commands além dos existentes
(`/exit`, `/clear`, `/model <nome>`):

- `/provider <nome>` / `/providers` — lista ou troca profiles configurados;
  a troca inicia uma conversa nova para não enviar contexto local à nuvem.
- `/models` — lista os modelos retornados pelo provider e marca o atual;
  na TUI, `/model ` abre uma lista filtrável e navegável.
- `/mode <auto|plan|accept-edits>` — troca o modo de interação também
  disponível via `Shift+Tab`.
- `/tools` — lista as 6 tools e se cada uma está "sempre permitida" nesta
  sessão, mais quantos comandos exatos de `run_command` foram aprovados com
  `a`.
- `/cwd` — mostra o diretório em que as tools estão de fato operando.
- `/continue` — quando a última rodada terminou com um `finish_reason`
  diferente de `stop`/`tool_calls` (tipicamente `length`, resposta cortada
  por `max_tokens`), reenvia a conversa com a instrução "Continue exactly
  where you stopped." em vez de fazer você reescrever o pedido. Se a última
  rodada terminou normalmente, avisa que não há nada para continuar.
- `/resume` — recarrega a última sessão salva para o provider ativo,
  substituindo a conversa atual. Útil para recuperar um chat fechado em uma
  invocação anterior do `insane`. A sessão é salva automaticamente ao sair
  do chat (TUI ou plain), e `/clear` remove o arquivo salvo para que um
  `--continue` posterior não ressuscite uma conversa que você apagou.

### TUI e modos de interação

Em um terminal interativo, o chat abre a TUI fullscreen. Enquanto um slash
command é digitado, uma paleta mostra comandos ou modelos compatíveis;
`↑`/`↓` seleciona e `Tab` completa. As setas `←`/`→`, `Home`, `End`,
`Backspace` e `Delete` editam na posição real do cursor; `Ctrl+←`/`Ctrl+→`
saltam entre palavras. `Alt+Enter` ou `Shift+Enter` insere uma nova linha.

`Shift+Tab` alterna entre:

- **AUTO** — lê livremente e pede confirmação antes de editar ou executar.
- **PLAN** — exploração somente leitura; escrita, edição e comandos ficam
  bloqueados.
- **ACCEPT EDITS** — aprova `write_file`/`edit_file` automaticamente, mas
  comandos shell continuam exigindo confirmação explícita.

As aprovações aparecem no rodapé, no lugar do editor. O painel mostra o
diff, comando ou alerta de segredo e opções navegáveis com `↑`/`↓`; `Enter`
confirma e `Esc` recusa. Use `PageUp`/`PageDown` ou a roda do mouse para
rolar previews longos.

### Robustez do loop agêntico

Alguns modelos (observado com variantes de `z-ai/glm-5.2`) anunciam uma ação
em texto e terminam o turno sem de fato chamar a tool correspondente, ou
emitem a chamada como JSON dentro do texto em vez de usar `tool_calls`
estruturado da API. Para isso:

- O system prompt do agente inclui SO/shell, cwd, data, modelo, um snapshot
  do projeto (capado em 150 entradas) e regras explícitas contra "anunciar
  sem agir"; `[agent] system_prompt_extra` no config anexa texto adicional.
- `max_tokens` default é 4096; `[agent] temperature` (default 0.2) controla
  a temperatura só do modo agente, com fallback para a `temperature` global
  se essa tiver sido configurada explicitamente.
- Se uma rodada termina com `finish_reason` diferente de `stop`/`tool_calls`,
  um aviso aparece em stderr (`warning: response ended early
  (finish_reason=length) -- type /continue to resume`); o valor também vai
  para o `--json` de `ask`.
- Com `[agent] lenient_tool_calls = true` (default), uma chamada de tool
  emitida como texto (JSON puro, `{"tool": ..., "parameters": ...}`, um bloco
  `<tool_call>...</tool_call>`, ou um bloco ```` ```json ```` final) é
  detectada e executada normalmente, com o traço `→ (recovered from text)
  nome(...)` em stderr. JSON incidental no meio de uma resposta em prosa
  nunca dispara essa recuperação.
- Em terminal interativo (stderr é TTY, sem `--quiet`), o chat mostra
  feedback de progresso: um spinner "model thinking... (round N/M)" enquanto
  aguarda o primeiro token, avisos quando o rate limiter está esperando um
  slot, um resumo por tool (`✓ read_file agent.rs (14.2 KB, 3ms)` / `✗
  edit_file ... (user denied)`), e uma linha final com métricas do turno
  (`-- 3 rounds | 2 tools | 1.9k tokens | 14s`).
- Na TUI, a roda do mouse rola a conversa e previews de aprovação. Como o
  app captura eventos de mouse para isso, alguns terminais exigem `Shift` ao
  arrastar para selecionar/copiar texto.

### Exemplo de sessão (ilustrativo)

```
$ insane chat
insane-cli chat (tools enabled) -- /exit, /clear, /model <name>, /tools, /cwd, /continue
> onde está definido o RateLimiter?
→ search_files(pattern="struct RateLimiter")
src/limiter.rs:44: pub struct RateLimiter {
Está definido em src/limiter.rs:44, um limitador de sliding-window-log...
> adiciona um comentário explicando o campo capacity
→ read_file(path="src/limiter.rs")
--- src/limiter.rs
+++ src/limiter.rs (proposed)
@@ -44,6 +44,7 @@
 pub struct RateLimiter {
+    /// Máximo de acquires por janela.
     capacity: usize,
Apply this edit to src/limiter.rs? [y/n/a] y
Pronto, adicionei o comentário.
```

### Modelos compatíveis com function calling

Nem todo modelo NIM suporta tool calling. Se a API rejeitar a requisição
(HTTP 400) com uma mensagem mencionando `tool`/`function`, o erro exibido já
inclui uma dica pronta:

```
error: permanent error (400): ...

hint: this model may not support function/tool calling; retry with a
tool-capable --model (e.g. meta/llama-3.3-70b-instruct or a `*-instruct`
model documented as tool-calling capable), or run `chat --no-tools` / `ask`
without `--tools`.
```

Esse erro é sempre tratado como permanente (nunca faz retry automático,
diferente de um 5xx/429 real).

## Flags globais

| Flag | Efeito |
|---|---|
| `--provider <profile>` | Escolhe um provider configurado para esta execução |
| `--model <m>` | Sobrepõe o modelo (config/env) para esta execução |
| `--json` | Emite um único objeto JSON em stdout (`{response, model, usage, timing_ms, rate_limiter, finish_reason?}`) em vez de texto |
| `--no-tools` | Desabilita o modo agente; funciona sem subcomando (`insane --no-tools`) e em `chat --no-tools` |
| `--stream` / `--no-stream` | Força ou desativa streaming (mutuamente exclusivos) |
| `--timeout <segundos>` | Timeout de requisição HTTP |
| `--quiet` | Suprime saída não essencial em stderr |
| `--verbose` (repetível) | Aumenta verbosidade dos logs em stderr (`-v`, `-vv`) |
| `--config <path>` | Usa um `config.toml` alternativo |
| `--no-cache` | Desativa o cache em disco nesta execução |
| `--yes` | Auto-confirma prompts apenas onde é seguro fazê-lo -- **nunca** substitui a confirmação de escrita quando há segredo detectado |

### `chat` flags

| Flag | Efeito |
|---|---|
| `--continue` (alias `--resume`, `--continue-last`) | Recarrega a última sessão salva para o provider ativo, restaurando modelo e histórico. A sessão é salva automaticamente ao sair do chat. |

## Precedência de configuração

`flags > variáveis de ambiente > arquivo config.toml > padrão`.

Arquivo: `{config_dir}/insane-cli/config.toml` (veja `insane config path`;
resolvido via crate `directories`, então segue a convenção de cada SO —
`%APPDATA%` no Windows, `~/.config` no Linux, `~/Library/Application
Support` no macOS). Exemplo comentado em `examples/config.example.toml`.

O arquivo usa profiles em `[providers.<nome>]`. Cada profile define
`kind`, `model`, `base_url`, autenticação, timeout e sua própria seção
`rate_limit`. Veja o arquivo completo em `examples/config.example.toml`.

Variáveis de ambiente reconhecidas: `NVIDIA_API_KEY`, `LMSTUDIO_API_KEY`,
`INSANE_PROVIDER`, `INSANE_MODEL`,
`INSANE_BASE_URL`, `INSANE_TIMEOUT`, `INSANE_MAX_TOKENS`,
`INSANE_TEMPERATURE`, `INSANE_STREAM`, `INSANE_RPM`,
`INSANE_MIN_INTERVAL` e `INSANE_LOG`.

`rpm` e `min_interval` são independentes e ambos são respeitados. Por
exemplo, `rpm = 40` com `min_interval = "1s"` impede mais de 40 requests em
qualquer minuto e também impede requests separados por menos de um segundo.
O endpoint público NVIDIA continua limitado a no máximo 40 RPM.

Configs antigas podem ser convertidas com `insane config migrate`; o arquivo
original recebe o sufixo `.pre-providers.bak`.

Use `insane doctor` para validar autenticação, endpoint e modelo.
`insane doctor --deep` também executa uma pequena chamada streaming e mede
TTFT.

### Guia rápido das novas configurações

```powershell
# 1. Converter uma configuração antiga (se houver)
insane config migrate

# 2. Criar/ajustar um profile local
insane config set providers.lmstudio.kind lmstudio
insane config set providers.lmstudio.model openai/gpt-oss-20b
insane config set providers.lmstudio.rate_limit.min_interval 1s

# 3. Torná-lo o provider padrão
insane config set active_provider lmstudio

# 4. Validar e abrir o chat
insane doctor --deep
insane
```

Para NVIDIA, salve a chave por profile com
`insane config set-key --provider nvidia`. Para escolher um provider apenas
na execução atual use `insane --provider lmstudio`; dentro da TUI use
`/providers` e `/provider lmstudio`.

## Exit codes

| Código | Significado |
|---|---|
| `0` | Sucesso |
| `1` | Erro genérico (permanente ou transitório após esgotar tentativas) |
| `2` | Uso inválido (prompt ausente, argumento malformado, escrita recusada/abortada) |
| `3` | Falha de autenticação (chave ausente ou inválida) |
| `4` | Orçamento do rate limiter esgotado |
| `130` | Cancelado (Ctrl+C) |

## Segurança

- **Denylist fixa de arquivos de chave/certificado**, sem bypass: `*.pem`,
  `*.key`, `id_rsa*`, `*.pfx`, `credentials*`, `secrets*` nunca são incluídos
  como contexto, mesmo com `--yes`. Arquivos `.env*` são permitidos.
- Respeita `.gitignore` do diretório atual, mais a lista `ignore` do
  `config.toml`.
- Leituras não abrem confirmação extra por conteúdo parecido com segredo.
  A proteção restante para leitura é o sandbox de diretório, `.gitignore` /
  `ignore` e a denylist de arquivos de chave/certificado.
- **Diff + confirmação antes de escrever**: `fix`, `refactor` e `test -o`
  sempre mostram um diff unificado antes de tocar o disco, e só escrevem após
  confirmação explícita (`y`/`yes`) -- em terminal não interativo a resposta
  padrão é sempre "não".
- **Escrita atômica com backup**: a escrita é feita em um arquivo temporário
  no mesmo diretório e promovida via rename atômico; um backup
  `<arquivo>.insane-bak` é criado antes de qualquer sobrescrita, restaurável
  com `--rollback` (que roda antes mesmo de resolver a chave de API ou abrir
  conexão de rede).
- **Redação de segredos em logs/erros**: qualquer string no formato
  `nvapi-...` é substituída por `nvapi-***` em toda saída de erro (inclusive
  mensagens vindas de uma resposta HTTP crua do provedor); o mesmo conjunto
  de padrões do detector de segredos é aplicado à saída de erro geral.
- **Limitação conhecida**: o rate limiter é mantido **por processo** (em
  memória). Se você rodar múltiplas instâncias do CLI simultaneamente (ex.
  vários terminais), cada uma tem seu próprio orçamento de 40 req/min -- elas
  não compartilham estado entre si. Documentado também em
  `docs/ARCHITECTURE.md`.

## Rate limiting

Limite: **40 requisições por minuto** (janela móvel, não um bucket que
permite rajadas na borda da janela) contra o endpoint público da NIM. Toda
requisição -- incluindo cada tentativa de retry -- passa pelo limiter antes
de sair. Quando o orçamento está esgotado, o comando **espera** (FIFO) até
haver uma vaga, em vez de falhar; `Retry-After` de um `429` é respeitado e
pausa globalmente o limiter pelo tempo indicado. Métricas (`used`,
`remaining`, `next_slot_in_ms`, `total_waited_ms`, `total_acquired`) aparecem
em `insane status` e em `--json`.

## Troubleshooting

- **"authentication error" / exit 3`**: nenhuma chave encontrada. Configure
  `NVIDIA_API_KEY` ou rode `insane config set-key`.
- **Demora muito antes de responder**: provavelmente o rate limiter está
  esperando uma vaga (rode `insane status` para ver `next_slot_in_ms`) ou o
  servidor respondeu `429`/`5xx` e o cliente está em backoff -- os avisos de
  retry aparecem em stderr com `--verbose`.
- **"filename matches the fixed denylist pattern"**: o arquivo passado em
  `-f`/`review`/`fix`/etc. bate com a denylist fixa (ex. `*.pem`, `*.key`,
  `id_rsa*`).
  Não há como contornar isso; renomeie/copie o conteúdo necessário para um
  arquivo com outro nome, se apropriado.
- **`config path` aponta para um lugar inesperado**: o caminho é derivado
  pela crate `directories` a partir do SO; use `--config <path>` para forçar
  um arquivo específico (útil também para testes/CI).
- **Erros vindos da API não citam o texto exato retornado pelo servidor**:
  qualquer coisa parecida com uma chave `nvapi-...` é sempre redigida antes
  de chegar ao terminal, mesmo que a mensagem de erro bruta do provedor a
  contivesse.

## Desenvolvimento

```bash
cargo test              # unit + integração (mock NIM local, sem rede externa)
cargo test -- --ignored # inclui o teste de carga pesado do rate limiter
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo build --release
```

Benchmarks reproduzíveis em `scripts/bench.ps1` / `scripts/bench.sh`;
números medidos em `docs/BENCHMARKS.md`.
