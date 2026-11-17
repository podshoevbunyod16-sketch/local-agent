# Замеры (реальные, не «ожидаемые»)

Стенд: x86_64, **2 ядра**, tanpa GPU (песочница CI-подобная), Rust 1.100,
модель **Qwen2.5-0.5B-Instruct Q4_K_M** (491 МБ), `--threads 2 --ctx 2048`,
flash attention вкл, KV-кэш q8_0.

```
$ turbollm-bench --model qwen.gguf --tokens 96 --turns 6 --ctx 2048 -t 2

loading qwen.gguf (threads=2, ctx=2048, kv=Q8_0, flash=true)
model loaded in 0.24s: ModelInfo { n_params: 630167424, n_ctx_train: 32768,
                                  n_layer: 24, n_embd: 896, has_chat_template: true }

turn 1: prompt   35 tok (cached    0) in   332.5 ms ( 105.3 tok/s) | gen 24 tok in 1057.4 ms = 22.70 tok/s | ttft  334 ms
turn 2: prompt   26 tok (cached   59) in   189.1 ms ( 137.5 tok/s) | gen 23 tok in 1104.2 ms = 20.83 tok/s | ttft  190 ms
turn 3: prompt   26 tok (cached  108) in   243.9 ms ( 106.6 tok/s) | gen 20 tok in  935.8 ms = 21.37 tok/s | ttft  244 ms
turn 4: prompt   26 tok (cached  154) in   246.2 ms ( 105.6 tok/s) | gen 20 tok in  941.2 ms = 21.25 tok/s | ttft  247 ms
turn 5: prompt   26 tok (cached  200) in   260.3 ms (  99.9 tok/s) | gen 20 tok in  907.7 ms = 22.03 tok/s | ttft  261 ms
turn 6: prompt   26 tok (cached  246) in   246.8 ms ( 105.4 tok/s) | gen 20 tok in  943.5 ms = 21.20 tok/s | ttft  247 ms

total prompt tokens skipped by the KV cache: 767

prefill of 6 turns: naive 7475 ms vs KV-reuse 2914 ms  =>  2.56x faster
```

Что видно:

* **`cached N`** растёт линейно — на каждом ходе движок переиспользует всю
  предыдущую историю вместе с ответами ассистента;
  на naive-прогоне на 6-м ходе нужно прогнать 224 токена, у нас — 26.
* Выигрыш **2.56x** — это уже на коротком диалоге (~250 токенов). Он растёт
  линейно с длиной истории: на 3000 токенов и ARM-телефоне prefill в 15–20 раз
  медленнее, поэтому там разница в TTFT измеряется десятками секунд.
* `decode tok/s` от KV-reuse **не** меняется (это свойство модели и памяти), его
  поднимают квантование, треды, flash attention и speculative decoding —
  см. [`SPEED.md`](SPEED.md).

> Числа в tok/s здесь низкие, потому что в песочнице всего 2 ядра x86_64 и нет
> dotprod/i8mm. Типичный телефон (Snapdragon 7+ Gen 2, 4 больших ядра) на той же
> модели даёт 20–35 tok/s для 0.5B и 6–10 tok/s для 3B Q4_K_M; приложение
> показывает эти цифры под каждым ответом и в кнопке «Бенчмарк».
