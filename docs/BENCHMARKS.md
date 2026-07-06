# insane-cli — Benchmarks

Números medidos de verdade (não estimados) na máquina abaixo, com o binário
`release` gerado por `cargo build --release`. Reproduzível via
`scripts/bench.ps1` (Windows) ou `scripts/bench.sh` (POSIX).

## Ambiente de medição

| Item | Valor |
|---|---|
| Data | 2026-07-04 (fase 3: números originais; fase 5: re-medido após o modo agente) |
| SO | Windows 11 Pro, 64 bits |
| CPU | 13th Gen Intel(R) Core(TM) i5-13600K (14 cores / 20 threads) |
| RAM | ~32 GB |
| Toolchain | `rustc 1.96.0`, `cargo 1.96.0` |
| Perfil | `release` (`lto = "thin"`, `codegen-units = 1`, `strip = true`, `panic = "abort"`) |
| Binário | `target\release\insane.exe` |
| Script | `scripts\bench.ps1 -N 30` |

## Startup latency

SPEC §9 alvo: `--help` < 50ms. `N = 30` execuções, tempos em milissegundos
(wall-clock, medidos pelo próprio script via `Stopwatch`, incluindo o custo
do processo `& insane.exe ... | Out-Null` do PowerShell -- portanto um
limite superior conservador do tempo real de execução do binário).

| Comando | Min | Mediana | Max | N |
|---|---|---|---|---|
| `insane --help` | 13.80 ms | **17.03 ms** | 182.56 ms | 30 |
| `insane config path` | 16.96 ms | **17.70 ms** | 23.56 ms | 30 |

Ambos continuam batendo a meta de < 50ms na mediana, com o mesmo perfil da
fase 3 (`config path` ligeiramente mais lento que `--help` na mediana,
consistente com o custo extra de resolver `directories::ProjectDirs` e
checar a existência do arquivo de config no disco) -- a adição do modo
agente (tools, permissões, loop em `src/agent.rs`) não degradou a latência de
startup de comandos que nem tocam esse código. O outlier de 182.56ms em
`--help` é atribuído a ruído do ambiente local (antivírus/Smart App Control
desta máquina reavaliando o binário recém-compilado na primeira execução
após um rebuild -- ver `docs/REPORT.md`), não ao binário em si: o mínimo
(13.80ms) e a mediana (17.03ms) ficam em linha com a fase 3.

## Memória de pico

Medido via `Get-Process` (`PeakWorkingSet64`) durante uma execução isolada de
`insane --help`:

| Métrica | Valor |
|---|---|
| Peak working set | **3 096 KB (≈3.02 MB)** |

Idêntico ao número da fase 3, apesar do binário maior (ver abaixo) --
consistente com um binário Rust estático sem runtime gerenciado: a maior
parte é o carregamento do próprio executável e das DLLs do sistema
(ucrtbase, kernel32, etc.), não alocação de heap do programa; o código extra
do modo agente só aloca quando de fato executado (tools, loop, permissões),
não no caminho de `--help`.

## Tamanho do binário

O binário cresceu com a fase 5 (modo agente: `src/agent.rs`, `src/tools/*`,
mais as crates `ignore` e `regex` que já eram usadas por `context`/`secrets`
mas agora também pelas tools -- nenhuma dependência nova foi adicionada):

| Métrica | Fase 3 | Fase 5 (com modo agente) |
|---|---|---|
| `target\release\insane.exe` | não medido nesta tabela | **5 550 080 bytes (≈5.29 MiB)** |

Ainda um único binário estático, sem runtime gerenciado, com o mesmo perfil
de release (`lto = "thin"`, `codegen-units = 1`, `strip = true`,
`panic = "abort"`).

## Latência contra o mock local

Não incluída como número fixo aqui (variaria por hardware e não agrega valor
frente aos benchmarks acima): as suítes de integração (`tests/rate_limit.rs`,
`tests/retry.rs`, `tests/streaming.rs`) já reportam tempos de execução reais
contra o mock NIM (axum) a cada `cargo test`, e servem como sinal contínuo de
regressão de latência do cliente/limiter/parser SSE.

## Como reproduzir

```powershell
cargo build --release
./scripts/bench.ps1 -N 20
```

```bash
cargo build --release
./scripts/bench.sh 20
```

Os números acima foram colados diretamente da seção "Summary" impressa pelo
script, sem edição manual.
