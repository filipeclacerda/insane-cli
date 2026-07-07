# Plano: estabilizar o teste `metrics_total_matches_concurrent_call_count`

## Contexto

O teste `tests/rate_limit.rs::metrics_total_matches_concurrent_call_count` está
flaky. Ele dispara 33 requisições concorrentes pelo `RateLimiter` real
(capacidade 8 / janela 750 ms) contra um mock NIM e então afirma que nenhuma
janela deslizante de 750 ms de *timestamps de chegada no servidor* contém mais
de 8 requisições. Em uma execução ele falhou com:

```
window starting at Instant { t: 33419.6279119s } contained 10 requests (capacity 8)
```

## Diagnóstico

- O `RateLimiter` (em `src/limiter.rs`) garante, corretamente, **no máximo
  `capacity` admissões por janela de tempo de admissão**. O timestamp que ele
  registra no log (`inner.log.push_back(now)`) é o instante de *admissão*.
- O teste, porém, mede **timestamps de chegada no servidor**
  (`RequestLog::record()` faz `Instant::now()` quando a requisição chega ao
  mock). Esse instante é `admissão + latência_da_requisição`.
- Como a latência varia, o segundo lote admitido (em ~T0+750 ms) pode chegar ao
  servidor *antes* dos retardatários do primeiro lote. Uma janela de 750 ms
  sobre tempos de chegada consegue então englobar requisições de dois lotes de
  admissão, contando até 10.
- Por isso o teste irmão com janela de 1 s (`concurrent_requests_never_exceed_capacity_in_any_window`)
  passa: a janela maior absorve o skew. E o teste unitário
  `never_exceeds_capacity_in_any_window` passa porque mede timestamps de
  *pós-acquire* (bem mais próximos do instante de admissão), não chegada no
  servidor.

**Conclusão:** não é um bug do limiter; é fragilidade do teste ao comparar
tempos de chegada contra uma janela derivada de tempos de admissão.

## Objetivo

Tornar o teste determinístico sem enfraquecer o garantia real do limiter e
sem alterar a API pública do `RateLimiter` (a proposta anterior de expor
`admission_log()` foi negada).

## Opções consideradas

1. **Tolerância na verificação server-side** — adicionar uma folga (ex.:
   `window + 50 ms`) ao `assert_no_window_exceeds` usado por este teste.
   Mantém o caráter end-to-end (continua medindo chegada no servidor) e
   absorve o skew de latência admissão→chegada. Mínima invasão.
2. **Marcar `#[ignore]`** — igual ao `heavy_load_never_exceeds_capacity` já
   ignorado. Evita falhas esporádicas no CI, mas perde cobertura no fluxo
   padrão.
3. **Aumentar a janela do teste** — usar 1 s em vez de 750 ms. Reduz a
   probabilidade de flake, mas não elimina a causa raiz.
4. **Reexecutar para confirmar flakiness** — diagnóstico, não correção.

## Plano escolhido: Opção 1 (tolerância)

É a que melhor preserva a intenção end-to-end do teste e corrige a causa do
flake sem mudar código de produção.

### Passos

1. **Estender `assert_no_window_exceeds`** em `tests/common/mod.rs`:
   - Adicionar um parâmetro `tolerance: Duration` (ou um novo helper
     `assert_no_window_exceeds_with_tolerance`) de forma que a janela
     verificada seja `window + tolerance`.
   - Manter o helper existente sem tolerância para os chamadores que medem
     timestamps de admissão/pós-acquire (onde não há skew).

2. **Atualizar `tests/rate_limit.rs`**:
   - Apenas os dois testes que medem *chegada no servidor*
     (`concurrent_requests_never_exceed_capacity_in_any_window` e
     `metrics_total_matches_concurrent_call_count`) passam a usar a versão
     com tolerância (ex.: `Duration::from_millis(50)`).
   - `heavy_load_never_exceeds_capacity` (já `#[ignore]`) pode permanecer
     como está ou também adotar a tolerância.

3. **Justificativa no comentário do teste**: explicar brevemente que a
   tolerância absorve o skew entre o instante de admissão (que o limiter
   controla) e o instante de chegada no servidor (que o teste mede).

4. **Validar**:
   - `cargo test --test rate_limit` repetido (ex.: 5–10x) para confirmar
     estabilidade.
   - `cargo test` completo para garantir nada regrediu.

### Pontos de atenção

- A tolerância **não** enfraquece a garantia do limiter (a garantia é sobre
  tempos de admissão, que continuam ≤ capacity por janela). Ela apenas
  reconhece que a medição por chegada no servidor introduz ruído de latência.
- Manter a tolerância pequena (50 ms) para que o teste ainda capture uma
  regressão real no limiter (ex.: admitir 16 por janela).
- Não alterar `src/limiter.rs` nem sua API pública.

## Fora do escopo

- Expor `admission_log()` no `RateLimiter` (proposta negada anteriormente).
- Reescrever o teste para medir timestamps de admissão.
- Mudar a política do limiter (FIFO, janela deslizante, etc.).
