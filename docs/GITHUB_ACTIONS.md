# Сборка APK на GitHub Actions — пошагово

Всё, что нужно: аккаунт GitHub и ~30 минут (из них 25 — первая сборка, дальше
2–4 минуты благодаря кэшу). Никакого Android Studio, Gradle и локального SDK не
требуется: сборка идёт на серверах GitHub, готовый APK скачивается оттуда.

---

## Шаг 1. Создать репозиторий и залить код

Скачайте `TurboLLM-src-0.2.0.zip` (он рядом с этим документом) и распакуйте.
Внутри — папка `turbollm` со всем проектом, включая `.github/workflows/`.

**Вариант А — через сайт GitHub (без установки git)**

1. <https://github.com/new> → Repository name: `turbollm` → **Private** или Public → **Create repository**.
2. На странице пустого репозитория: «uploading an existing file».
3. Перетащите **содержимое** папки `turbollm` так, чтобы в корне репозитория
   лежали `Cargo.toml`, `crates/`, `android/`, `scripts/`, `README.md`.
   ⚠️ Не заливайте саму папку `turbollm` как единственный элемент — иначе
   проектом станет `turbollm/turbollm`.
4. ⚠️ **Проверьте, что попала папка `.github`** — при перетаскивании в браузере
   она часто не загружается, а без неё Actions ничего не увидит. Если её нет:
   **Add file → Create new file**, имя `/.github/workflows/build-apk.yml`,
   вставьте содержимое одноимённого файла из архива. Затем так же создайте
   `.github/workflows/release.yml`.
5. Commit changes.

**Вариант Б — через git (надёжнее, ничего не теряется)**

```bash
unzip TurboLLM-src-0.2.0.zip
cd turbollm
git init -b main
git add -A
git commit -m "TurboLLM 0.2.0"
git remote add origin https://github.com/<ваш_логин>/turbollm.git
git push -u origin main
```

Git сохраняет папку `.github` всегда — это главная причина предпочесть этот путь.

---

## Шаг 2. Разрешить Actions

1. В репозитории: **Settings → Actions → General**.
2. «Actions permissions» → **Allow all actions and reusable workflows** → Save.
3. Ниже, «Workflow permissions» → **Read and write permissions** (нужно, чтобы
   релизный воркфлоу мог прикрепить APK к релизу) → Save.

Первая сборка запустится сама на push в `main`. Если нет — **Actions → Build APK
→ Run workflow** (кнопка справа) → Run workflow.

---

## Шаг 3. Забрать APK

**Actions → Build APK → верхний (зелёный) запуск → внизу раздел Artifacts:**

| Артефакт | Что внутри |
|---|---|
| `apk-arm64-v8a` | APK для всех современных телефонов (основной, ~3 МБ) |
| `apk-armeabi-v7a` | 32-бит ARM, очень старые устройства |
| `apk-x86_64` | эмулятор Android Studio |
| `TurboLLM-universal-apk` | один APK со всеми ABI (~12 МБ) |
| `benchmark-log` | лог бенчмарка: реальные tok/s на сервере GitHub |

Артефакт скачивается как ZIP — распакуйте, внутри `TurboLLM-0.2.0-release.apk`.
Ставьте на телефон: `adb install -r TurboLLM-0.2.0-release.apk`.

⚠️ Артефакты Actions хранятся 90 дней. Для «вечного» файла — см. шаг 5
(релиз).

---

## Шаг 4. Параметры запуска вручную

**Actions → Build APK → Run workflow** открывает два поля:

| Поле | Значение по умолчанию | Зачем менять |
|---|---|---|
| `abis` | `arm64-v8a armeabi-v7a x86_64` | оставьте только `arm64-v8a` — сборка втрое короче |
| `variant` | `release` | `debug` даёт более подробные логи, но APK больше и медленнее |

Ветку выбирать не нужно, если пушите в `main` (по умолчанию workflow реагирует
на `main`; для другой ветки поправьте `branches: [main]` в
`.github/workflows/build-apk.yml`).

### Сколько это займёт

| Этап | Первый раз | С кэшем |
|---|---|---|
| Host-тесты (Rust + бенчмарк реальной модели) | 8–12 мин | 3–5 мин |
| Один ABI (компиляция llama.cpp под ARM) | 20–25 мин | 2–4 мин |
| Universal APK (3 ABI) | 40–50 мин | 6–10 мин |

Компиляция llama.cpp кэшируется между запусками (`Swatinem/rust-cache`), поэтому
повторные сборки быстрые. Лимит бесплатного тарифа — 2000 минут/месяц для
приватных репозиториев; для публичных минуты не считаются.

---

## Шаг 5. Свой ключ подписи и «настоящий» релиз

Пока секретов нет, скрипт генерирует одноразовый debug-ключ. APK ставится и
работает, но **обновить им более раннюю установку нельзя** (подписи не совпадут),
и ключ меняется от запуска к запуску. Для регулярных сборок сделайте постоянный:

```bash
keytool -genkeypair -v -keystore release.keystore -alias turbollm \
  -keyalg RSA -keysize 2048 -validity 10000 \
  -storepass ПАРОЛЬ -keypass ПАРОЛЬ \
  -dname "CN=TurboLLM, O=TurboLLM, C=TJ"

base64 -w0 release.keystore > release.keystore.b64   # macOS: base64 -i
```

**Settings → Secrets and variables → Actions → New repository secret**, четыре
секрета:

| Имя | Значение |
|---|---|
| `KEYSTORE_BASE64` | содержимое `release.keystore.b64` (одна строка) |
| `KEYSTORE_PASS` | пароль хранилища |
| `KEY_ALIAS` | `turbollm` |
| `KEY_PASS` | пароль ключа |

`release.keystore` в репозиторий не коммитьте (`.gitignore` его не содержит —
проверьте сами).

Дальше релиз делается тегом:

```bash
git tag v0.2.0
git push origin v0.2.0
```

Запустится `.github/workflows/release.yml`: соберёт universal APK, подпишет
вашим ключом и создаст **GitHub Release** с прикреплённым файлом — постоянная
ссылка вида `https://github.com/<логин>/turbollm/releases/download/v0.2.0/TurboLLM-v0.2.0-universal.apk`.

---

## Шаг 6. Если сборка упала

Откройте упавший job → раскройте красный шаг. Что чаще всего:

| Ошибка в логе | Причина | Решение |
|---|---|---|
| `sdkmanager: command not found` | на раннере нет cmdline-tools | в шаге установки замените путь на `${ANDROID_HOME}/cmdline-tools/latest/bin/sdkmanager` (современные образы `ubuntu-latest` используют `ANDROID_HOME`) |
| `NDK not found at .../ndk/27.2.12479018` | версия NDK не установилась | проверьте шаг «Install Android NDK + cmake», он должен завершиться без ошибок; при необходимости поставьте `NDK_VERSION: 26.1.10909125` в `env:` |
| `error: failed to run custom build command for llama-cpp-sys-2` | нет cmake/clang | шаг «Install cmake + clang» есть только в `test-host`; добавьте его в job сборки APK |
| `No space left on device` | кэш + 3 ABI на 14 ГБ диска | собирайте по одному ABI (`abis: arm64-v8a`) |
| `out of memory` при LTO | `lto = "fat"` съедает RAM | в `Cargo.toml` временно `lto = "thin"` |
| Job «Cancelled» через 6 часов | таймаут GitHub | уменьшите число ABI |
| Actions вообще не запустились | папка `.github` не попала в репозиторий | шаг 1, пункт 4 |

Локально повторить сборку ровно так же:

```bash
export ANDROID_SDK_ROOT=$HOME/Android/Sdk
export ANDROID_NDK_HOME=$ANDROID_SDK_ROOT/ndk/27.2.12479018
cargo install cargo-ndk

VERSION_NAME=0.2.0 ABIS="arm64-v8a" GGML_CPU_ARM_ARCH=armv8.2-a+dotprod \
  bash scripts/build-apk.sh
# -> build/TurboLLM-0.2.0-release.apk
```

---

## Что именно делают воркфлоу

`build-apk.yml`, три job'а:

1. **test-host** — `cargo fmt --check`, `cargo clippy -D warnings`, затем
   скачивает Qwen2.5-0.5B (490 МБ) и **реально генерирует токены** на сервере.
   Печатает tok/s в лог и в артефакт `benchmark-log`. Это защита от «собралось,
   но не работает».
2. **build-apk** — матрица из трёх ABI: ставит NDK 27.2 + cmake, `cargo-ndk`,
   зовёт `scripts/build-apk.sh`, кладёт APK в артефакты.
3. **universal-apk** — собирает все ABI в один APK; на теге `v*` дополнительно
   создаёт GitHub Release.

`release.yml` — отдельный путь для релизов: постоянный ключ из секретов,
переименование в `TurboLLM-<тег>-universal.apk`, прикрепление к Release.

---

## Частые вопросы

**Модель входит в APK?** Нет. Она весит ~490 МБ. После установки положите
`qwen.gguf` в `Android/data/ai.turbollm.app/files/models/` (или откройте `.gguf`
из файлового менеджера — приложение подхватит). Ссылка на модель есть в
`README.md`.

**Почему не Gradle?** Потому что APK собирается теми же утилитами, что и внутри
AGP, но без скачивания сотен мегабайт зависимостей — 2–4 минуты против 10–15, и
никакой «лотереи» с версиями AGP. Подробности — `README.md`, раздел «Почему без
Gradle».

**Соберётся ли на бесплатном аккаунте?** Да, для публичного репозитория минуты
не расходуются; для приватного ~25 минут на первую сборку одного ABI.

**Можно ли собрать в другом CI (GitLab, Drone)?** Да, вся сборка — один скрипт
`scripts/build-apk.sh`; нужны только JDK 21, Rust с android-таргетами, Android
SDK/NDK и `cargo-ndk`.
