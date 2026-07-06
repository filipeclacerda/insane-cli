# insane-cli — SPEC Addendum 2: Robustez do agente, feedback e TUI

Motivação (relato real do usuário, chat com `z-ai/glm-5.2`): o modelo leu um arquivo via tool, anunciou "vou criar o plano" em texto e o turno **encerrou silenciosamente** — sem tool call, sem aviso. Além disso, o usuário não tem feedback do que está acontecendo durante o turno. O chat é a feature principal do CLI.

## Parte A — Robustez do loop agêntico e contexto do modelo

### A1. System prompt enriquecido (src/agent.rs)
O system prompt do agente passa a incluir:
- SO + shell (`windows/powershell`, `unix/sh`), cwd absoluto, data atual, nome do modelo em uso;
- snapshot do projeto: listagem de arquivos do cwd (via walker do list_files, respeitando ignore/denylist), cap 150 entradas, indicando `/` para dirs e `(+N more)` se estourar — dá contexto imediato sem gastar uma rodada;
- regras comportamentais explícitas (inglês, imperativas):
  - "You are an agent. Keep working until the user's request is fully resolved. NEVER end your turn right after announcing an action — announcing without calling the corresponding tool is a critical failure. If you say you will read/create/edit/run something, CALL THE TOOL in this same turn."
  - "When asked to create or modify a file, actually create/modify it with write_file/edit_file — do not print the would-be content as your answer unless the user asked to see it first."
  - "Prefer edit_file for small changes; write_file for new files. Verify your work with run_command when a test/build command is available and relevant."
  - "If a tool returns an error, adapt and try a different approach instead of giving up."
  - "Respond in the user's language; keep code and file contents in their original language."
- `config [agent] system_prompt_extra = ""` — texto adicional do usuário anexado ao final.

### A2. Parâmetros de geração
- `max_tokens` default sobe para **4096** (config já existente; era baixo/ausente). `temperature` default 0.2 para o modo agente (config `agent.temperature`, fallback ao global). Sempre enviar ambos no request.

### A3. finish_reason visível e recuperação
- Ao fim de cada rodada, se `finish_reason` não for `stop` nem `tool_calls`, exibir aviso claro no stderr: `⚠ response ended early (finish_reason=length) — type /continue to resume`.
- `finish_reason == "length"` no chat: oferecer continuação — novo slash command `/continue` reenvia com instrução "Continue exactly where you stopped." (sem repetir o texto já emitido).
- Registrar `finish_reason` no modo `--json`.

### A4. Fallback para tool call emitida como texto (modelos não-conformes, ex. GLM)
Alguns modelos emitem a chamada como TEXTO no content em vez de `tool_calls` estruturado. Após uma rodada com `finish_reason=stop` e sem tool_calls, aplicar detecção lenient sobre o content acumulado:
- content é (ou contém como único bloco/última linha) um JSON `{"name": "<tool conhecida>", "arguments": {...}}` ou `{"tool": ..., "parameters": ...}`; ou bloco `<tool_call>...json...</tool_call>`;
- se casar: tratar como tool call (id sintético `text_call_{n}`), logar `→ (recovered from text) nome(...)` e seguir o loop normalmente. O texto do "anúncio" que veio antes é impresso normalmente.
- Config `agent.lenient_tool_calls = true` (default). Testes unitários com os formatos acima e com falsos-positivos (JSON qualquer no meio de prosa NÃO dispara).

### A5. Feedback durante o turno (modo linha, pré-TUI)
- Spinner/status no stderr enquanto aguarda o primeiro token de cada rodada: `⠋ model thinking… (round 2/20)`; apagar a linha quando o texto começar. Detectar TTY; sem TTY, silencioso.
- Se o rate limiter estiver esperando slot: `⏳ rate limit: waiting 12s (38/40 used)`.
- Após cada tool: linha de resumo no stderr: `✓ read_file agent.rs (14.2 KB, 3ms)` / `✗ edit_file … (user denied)` / `✓ run_command "cargo test" (exit 0, 8.4s)`.
- Ao final do turno: linha discreta com métricas: `— 3 rounds · 2 tools · 1.9k tokens · 14s`.
- `--quiet` suprime tudo isso.

### A6. `insane` sem argumentos abre o chat
Subcomando do clap vira opcional; ausência = `chat` (com tools). `insane --no-tools` também aceito nesse caso.

## Parte B — TUI fullscreen (estilo opencode)

### B1. Comportamento
- `insane` / `insane chat` em terminal interativo (stdout E stdin TTY) abre TUI fullscreen (alternate screen). Flag `--plain` força o modo linha atual (que permanece para pipes/CI e como fallback se o terminal não suportar).
- Crates: `ratatui` + `crossterm` (já é dependência transitiva comum; manter versões estáveis atuais).

### B2. Layout
```
┌─ insane-cli ─ model: z-ai/glm-5.2 ─ cwd: …\insane-cli ──────────┐
│ (viewport rolável: conversa)                                     │
│  ▌you: leia o agent.rs e crie um plano…                          │
│  ▌assistant: Vou ler o arquivo…                                  │
│  ├─ ✓ read_file agent.rs (14.2 KB)                               │
│  ├─ ✓ write_file PLANO.md (+120 lines)  [diff foi confirmado]    │
│  ▌assistant: Criei o plano com…                                  │
├───────────────────────────────────────────────────────────────────┤
│ > input (multi-linha com wrap; Enter envia, Shift+Enter nova linha│
│   se o terminal reportar; senão Alt+Enter)                        │
├─ status: ⠋ round 2/20 · rate 38/40 · 1.9k tok · Ctrl+C cancel ───┤
```
- Conversa: mensagens do usuário e do assistente com prefixo/cor distintos; tool calls como blocos compactos com ✓/✗; texto do assistente atualiza em streaming (re-render por chunk com throttle ~30fps).
- Scroll: PgUp/PgDn e roda do mouse; auto-scroll para o fim enquanto streaming, a menos que o usuário tenha rolado para cima (retomar com End).
- Barra de status: modelo, rodada, métricas do rate limiter, tokens, dica de teclas; spinner quando aguardando modelo.

### B3. Confirmações e diffs na TUI
- Confirmação de tool (write/edit/run/segredo) vira modal centrado: mostra diff (write/edit, com cores add/del, rolável se grande) ou comando (run_command); teclas `y`/`n`/`a`/`Esc`(=n). O modal BLOQUEIA o loop até resposta — mesma semântica y/n/a da SPEC-AGENT §3, mesmo objeto `Permissions`.
- Implementação: o loop agêntico não pode chamar prompt de stdin direto na TUI. Introduzir abstração `trait Ui` (ou canal de eventos): `fn confirm(req) -> Decision`, `fn tool_trace(...)`, `fn stream_text(...)`, `fn status(...)`. Modo linha implementa com stderr/stdin (comportamento atual); TUI implementa com modal/render. `Permissions` passa a receber o `Ui` em vez de tocar o terminal.

### B4. Teclas e comandos
- Enter envia; input vazio + Ctrl+D ou `/exit` sai; Ctrl+C durante turno cancela o turno (mantém sessão), Ctrl+C com input vazio e sem turno ativo sai; Ctrl+L = /clear.
- Slash commands existentes funcionam (/exit /clear /model /tools /cwd /continue) + `/help`.
- Histórico de inputs com ↑/↓ (em memória, sessão apenas).

### B5. Robustez
- Restaurar terminal SEMPRE (RAII guard + hook de panic que desfaz raw mode/alt screen antes de imprimir o panic redigido).
- Resize tratado; larguras mínimas degradam com wrap.
- stdout do modelo: na TUI nada vai para stdout; ao sair, opcionalmente nada é reimpresso (a conversa vive na TUI). Modo linha continua stdout=resposta/stderr=logs.

## Testes
- A1/A2: unit — system prompt contém cwd/SO/listagem cap 150; params presentes no request (mock inspeciona body).
- A3: mock com finish_reason=length → aviso emitido; /continue envia mensagem de continuação.
- A4: unit do detector lenient (positivos: JSON puro, bloco <tool_call>, {"tool":...}; negativos: prosa com JSON incidental) + integração: mock roteirizado responde tool-call-em-texto → loop executa a tool e completa.
- A5: smoke — resumos de tool no stderr (capturável no modo linha in-process).
- A6: clap — `insane` sem args resolve para chat (unit no parser; sem TTY cai no modo linha, que sem stdin encerra graciosamente).
- B: TUI é exercitada minimamente (funções de layout/formatação puras testáveis: formatação de blocos de tool, wrap, redução de diff); interação completa é validação manual do usuário — documentar roteiro de teste manual no REPORT.
