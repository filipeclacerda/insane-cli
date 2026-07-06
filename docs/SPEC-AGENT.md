# insane-cli — SPEC Addendum: Modo Agente (tool calling no chat)

Extensão da SPEC.md. Objetivo: transformar `insane chat` em um agente interativo estilo opencode/Claude Code — o modelo chama ferramentas para listar/ler/editar arquivos e executar comandos, com aprovação do usuário para ações perigosas. Prioridades inalteradas: segurança > rate limit > correção > desempenho > UX.

## 1. API — function calling (OpenAI-compatível na NIM)

- `ChatRequest` ganha `tools: Option<Vec<ToolDef>>` e `tool_choice: Option<String>` ("auto").
  - `ToolDef = { "type": "function", "function": { "name", "description", "parameters": <JSON Schema> } }`
- Mensagens: `Message` precisa suportar:
  - assistant com `tool_calls: [{ "id", "type": "function", "function": { "name", "arguments": String(JSON) } }]` e `content` possivelmente nulo;
  - role `tool` com `tool_call_id` e `content` (resultado serializado, string).
- Não-streaming: `choices[0].message.tool_calls`, `finish_reason == "tool_calls"`.
- Streaming: deltas com `choices[0].delta.tool_calls[{index, id?, function:{name?, arguments-fragmento}}]` — acumular por `index` concatenando `arguments`; fim da rodada quando `finish_reason == "tool_calls"`.
- Nem todo modelo NIM suporta tools. Se a API retornar 400 mencionando tools/function: erro claro sugerindo `--model` compatível (ex.: `meta/llama-3.3-70b-instruct`, `qwen/...instruct`). Não fazer retry (permanente).
- Cada rodada do loop é 1 requisição → passa pelo rate limiter normalmente.

## 2. Ferramentas expostas ao modelo (src/tools/)

Todas operam APENAS dentro do cwd (canonicalizar path; rejeitar escapes por `..`, absolutos fora do cwd e symlinks que saem do cwd — erro devolvido ao modelo, não ao usuário).

| Tool | Parâmetros | Risco | Comportamento |
|---|---|---|---|
| `list_files` | `path?` (default "."), `max_entries?` | leitura | Lista recursiva respeitando .gitignore + ignore do config + denylist (crate `ignore`); cap 500 entradas; indica diretórios com `/`. |
| `read_file` | `path`, `start_line?`, `end_line?` | leitura→rede | Denylist fixa bloqueia (erro ao modelo). Cap `max_context_bytes`. **Secret scan antes de devolver**: se houver achados, pedir confirmação ao usuário (mostra tipo+linha); recusa devolve erro "user denied" ao modelo. |
| `search_files` | `pattern` (regex), `path?`, `max_results?` | leitura→rede | Grep sobre arquivos não-ignorados; cap 100 linhas; mesmas regras do read (secret scan no resultado). |
| `write_file` | `path`, `content` | escrita | Mostra diff (novo arquivo = diff contra vazio) e pede confirmação; escrita atômica + backup (fileops existente). |
| `edit_file` | `path`, `old_string`, `new_string`, `replace_all?` | escrita | `old_string` deve ser único no arquivo (a menos de `replace_all`); não encontrado/ambíguo → erro ao modelo. Diff + confirmação + atômico + backup. |
| `run_command` | `command`, `timeout_secs?` (cap 300) | execução | **Sempre** confirma (mostra o comando). Executa via `powershell -NoProfile -Command` no Windows, `sh -c` no Unix, no cwd. Captura stdout+stderr mesclados, cap 32KiB (truncar com aviso), retorna também exit code. Kill no timeout. |

- Resultado de tool → `role:"tool"` com `tool_call_id`, content string (JSON `{ok, output|error}` compacto).
- Erros de tool NUNCA abortam a sessão: viram resultado de erro para o modelo continuar.

## 3. Permissões e confirmações

- Prompt de confirmação no stderr, lendo do terminal: opções `y` (sim), `n` (não), `a` (sempre nesta sessão **para esta tool**). `a` não existe para `run_command` de comandos diferentes — para shell, `a` aprova apenas o comando idêntico repetido.
- stdin não-TTY → recusa automática (mesma regra do fileops atual).
- `--yolo` NÃO será implementado. Não criar nenhum bypass global.
- Leituras (`list_files`) não confirmam. `read_file`/`search_files` só confirmam quando há segredo detectado (conteúdo vai para a API).
- Sessão exibe cada chamada de tool de forma visível e distinta da resposta: linha `→ read_file src/main.rs` (stderr, com cor se TTY), e para write/edit o diff antes da confirmação.

## 4. Loop agêntico (src/agent.rs)

```
loop (máx N rodadas, default 20, config `agent.max_rounds`):
  resposta = client.chat_stream(messages + tools)     # streaming; texto impresso incremental
  se delta de texto: imprimir chunk a chunk (stdout)
  se finish_reason == "tool_calls":
      para cada tool_call (sequencial, na ordem):
          exibir → nome(args resumidos)
          executar com permissões acima
          anexar mensagem role:"tool"
      continuar loop
  senão: fim do turno; volta ao prompt do usuário
Ctrl+C durante o loop: aborta a rodada em voo, descarta tool_calls pendentes, volta ao prompt (NÃO sai do chat); Ctrl+C no prompt vazio sai (130).
```

- Histórico: reutilizar `session.rs` (trim por bytes preserva pares tool_call/tool result íntegros — nunca deixar `tool` órfão de seu assistant/tool_calls; se trim cortaria no meio, remover o par inteiro).
- System prompt do agente: descreve as tools, o cwd, SO, instrui a editar via tools e a pedir permissão implícita (a UI cuida). Conciso, em inglês.

## 5. CLI/UX

- `chat` ganha tools **habilitadas por padrão**; `--no-tools` desabilita (comportamento antigo).
- Novos slash commands na sessão: `/tools` (lista + estado de "sempre permitir"), `/cwd`, além dos existentes `/exit /clear /model`.
- `ask` ganha `--tools` opcional (loop agêntico não-interativo é perigoso: com stdin não-TTY toda escrita/shell é recusada automaticamente — comportamento correto e documentado).
- Saída: texto do modelo em stdout; tool traces/diffs/confirmations em stderr.

## 6. Testes (fase de testes)

Mock NIM estendido: responder com `tool_calls` (não-stream e stream com arguments fragmentado em vários deltas), depois resposta final; roteiro configurável por teste. Testar: acumulação de deltas de tool_calls; loop executa tool e envia role:"tool" com id correto (mock valida o corpo recebido); sandbox (path fora do cwd → erro ao modelo, arquivo intocado); denylist em read_file; edit_file old_string ambíguo; recusa não-TTY de write/edit/run_command mantém arquivo intacto; max_rounds interrompe loop infinito de tools; Ctrl+C não testável em CI → testar cancelamento via timeout/token onde possível; secret scan em read_file (não-TTY → erro "user denied", segredo não aparece no corpo enviado ao mock).
