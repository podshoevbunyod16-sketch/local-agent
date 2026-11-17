# TurboLLM — локальный ИИ на Android, ядро на Rust

Приложение, которое запускает **GGUF-модели прямо на телефоне** (без интернета,
без облака) и делает это заметно быстрее типичных «локальных чатов»:

* **Rust-ядро** (обёртка над llama.cpp/GGML) → `libturbollm_core.so` для
  `arm64-v8a`, `armeabi-v7a`, `x86_64`, `x86`;
* **JNI-мост** со стримингом токенов (текст появляется по мере генерации);
* **KV-cache prefix reuse**: история чата не пересчитывается на каждом ходе
  (2.5x быстрее prefill уже на 6 ходах, до 20–50x на длинных диалогах);
* **Flash attention**, **q8_0/q4_0 KV-кэш**, **mmap** весов, треды по big-ядрам;
* **агент с инструментами** (v0.2): встроенные `now`/`calc`/`device_info`,
  плагины на HTTP, MCP-серверы, два режима — `auto` (нативные tool-calls) и
  **ReAct** с циклом Thought → Action → Observation, который работает до успеха;
* сборка **в GitHub Actions в APK без Gradle** — только `cargo-ndk`, `aapt2`,
  `javac`, `d8`, `apksigner` (быстро, без AGP/Maven-лотереи).

Подробно про ускорение: [`docs/SPEED.md`](docs/SPEED.md) ·
замеры: [`docs/BENCHMARK.md`](docs/BENCHMARK.md) ·
альтернативные движки: [`docs/ALTERNATIVES.md`](docs/ALTERNATIVES.md) ·
архитектура: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) ·
агент/ReAct: [`docs/AGENT.md`](docs/AGENT.md) ·
плагины: [`docs/PLUGINS.md`](docs/PLUGINS.md) ·
MCP: [`docs/MCP.md`](docs/MCP.md).

---

## Быстрый старт

```bash
# 1. Модель (пример: Qwen2.5-0.5B-Instruct Q4_K_M, ~490 МБ)
curl -L -o qwen.gguf \
  https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/main/qwen2.5-0.5b-instruct-q4_k_m.gguf

# 2. Проверить движок на компьютере (без Android вообще)
cargo run --release --no-default-features --bin turbollm-bench -- \
  --model qwen.gguf --tokens 128 --turns 6 --compare

# 3. Собрать APK (нужны Android SDK+NDK, JDK 17+, Rust, cargo-ndk)
export ANDROID_SDK_ROOT=$HOME/Android/Sdk
export ANDROID_NDK_HOME=$ANDROID_SDK_ROOT/ndk/27.2.12479018
cargo install cargo-ndk
ABIS="arm64-v8a" bash scripts/build-apk.sh
# -> build/TurboLLM-0.2.0-release.apk
```

Готовые APK (собраны и проверены в этой среде): `../dist/`

* `TurboLLM-0.2.0-arm64.apk` (3.2 МБ) — arm64-v8a, `armv8.2-a+dotprod` (основной)
* `TurboLLM-0.1.0-universal.apk` (12 МБ) — все ABI (предыдущая версия)

Установка на телефон:

```bash
adb install -r build/TurboLLM-0.2.0-release.apk
adb push qwen.gguf /sdcard/Android/data/ai.turbollm.app/files/models/
```

В приложении: «Модель» → выбрать `qwen.gguf` → загрузится и можно общаться.
Модель также можно открыть из файлового менеджера (`.gguf`).
Кнопка «Бенчмарк» в настройках показывает `tok/s` и `prefill tok/s`.

### Агент (v0.2)

1. «Меню → Инструменты» — посмотреть включённые инструменты, поставить галочку
   **ReAct**, установить плагин или добавить MCP-сервер.
2. Спросите: «Сколько будет 123 * 45 + 67?» — в ответе появится чип
   `calc(...) → 5602`, строка статистики и финальный ответ из наблюдения.
3. Пошагово: `docs/AGENT.md`; свои инструменты — `docs/PLUGINS.md`;
   MCP — `docs/MCP.md`.

## GitHub Actions

`.github/workflows/build-apk.yml`:

| Job | Что делает |
|---|---|
| `test-host` | `fmt` + `clippy`, собирает `turbollm-bench`, скачивает Qwen2.5-0.5B и **реально генерирует** токены, печатает tok/s |
| `build-apk` | матрица `arm64-v8a` / `armeabi-v7a` / `x86_64`, по APK на ABI |
| `universal-apk` | один APK со всеми ABI, при теге `v*` прикрепляется к GitHub Release |

Запуск вручную: **Actions → Build APK → Run workflow** (можно передать список ABI и variant).

Пошаговая инструкция «залить на GitHub и собрать APK» — [`docs/GITHUB_ACTIONS.md`](docs/GITHUB_ACTIONS.md).

## Структура

```
crates/turbollm-core/        Rust-ядро + JNI
  src/engine.rs              загрузка GGUF, контекст, flash attn, KV-кэш
  src/session.rs             шаблон чата, KV-cache reuse, стриминг
  src/api.rs                 параметры генерации, статистика
  src/jni_bridge.rs          JNI-функции для Java
  src/tools.rs               реестр инструментов, JSON-схемы, GBNF
  src/builtin_tools.rs       now / calc / device_info
  src/agent.rs               цикл агента: режимы auto и re_act, ReAct-парсер
  src/mcp.rs                 MCP-клиент (JSON-RPC, tools/list, tools/call)
  src/plugins.rs             манифест плагина + HTTP-инструменты
  src/bin/bench.rs           хостовый бенчмарк (CI)
  src/bin/agent_smoke.rs     прогон агента на реальной модели
android/app/src/main/        Java-приложение (без AndroidX)
  java/ai/turbollm/app/      MainActivity, NativeBridge, ToolExecutor,
                             PluginTools, Settings, Service
  res/, AndroidManifest.xml
scripts/build-apk.sh         сборка APK без Gradle
.github/workflows/           CI
docs/SPEED.md                что реально ускоряет токены
docs/AGENT.md                агент, ReAct, политика «до успеха»
docs/PLUGINS.md              плагины-инструменты на HTTP
docs/MCP.md                  MCP-клиент
docs/GITHUB_ACTIONS.md       сборка на GitHub Actions, шаг за шагом
```

## Почему без Gradle

Gradle + AGP тянут сотни мегабайт зависимостей и требуют сети к `google()`.
Нам не нужны ни AndroidX, ни Compose: UI — платформенные View, всё тяжёлое — в
Rust. Поэтому `scripts/build-apk.sh` делает ровно то, что делает AGP, но
явно и предсказуемо:

```
cargo ndk → libturbollm_core.so
aapt2 compile/link → resources.arsc + R.java
javac (classpath=android.jar) → .class
d8 → classes.dex
zip + zipalign + apksigner → APK
```

Лямбды десугариваются `d8` (minSdk 26), поэтому Java 8-синтаксис работает.
Если позже понадобится AndroidX/Compose — рядом лежит `android/settings.gradle.kts`
с эквивалентной Gradle-конфигурацией (AGP 8.7), можно переключиться без
изменения Rust-части.

## Ограничения / что дальше

* `speculative decoding` (MTP/draft-модель) — интерфейс заложен
  (`GenParams::use_speculative`, `draft_tokens`), включается, когда нужна
  draft-модель с тем же токенизатором; см. `docs/SPEED.md` §3.
* GPU-бэкенд: добавьте фичу `opencl` (Adreno/Mali) или `vulkan` в
  `llama-cpp-2` — приложение уже передаёт `n_gpu_layers`.
* MCP: только Streamable HTTP; stdio-транспорт и OAuth не поддерживаются
  (`docs/MCP.md`).
* Модели качаются вручную (или через SAF-импорт в приложении); встроенного
  менеджера загрузок нет намеренно — не нужны сетевые разрешения для инференса.

## Версии

| Версия | Что нового |
|---|---|
| 0.2.0 | Агент: инструменты, плагины, MCP, ReAct-цикл «до успеха»; исправлено падение при выгрузке модели (UAF) и утечка JNI-ссылок на каждом токене |
| 0.1.0 | Ядро: GGUF, KV-cache reuse, стриминг, бенчмарк, сборка APK без Gradle |

## Лицензии

Код — MIT OR Apache-2.0. llama.cpp/GGML — MIT (см. `llama-cpp-sys-2`).
Модели — по лицензии их авторов.
