# MCP: подключение внешних серверов инструментов

TurboLLM умеет выступать MCP-клиентом: подключиться к серверу по HTTP, получить
список инструментов и вызывать их так же, как встроенные. Клиент написан на Rust
(`crates/turbollm-core/src/mcp.rs`), транспорт — в Kotlin, потому что сетевые
запросы на Android удобнее делать стандартным `HttpURLConnection`.

## 1. Что поддерживается

| Возможность | Статус |
|---|---|
| Транспорт Streamable HTTP (`POST` JSON-RPC 2.0) | да |
| `initialize` / `notifications/initialized` | да |
| `tools/list` (включая пагинацию по `nextCursor`) | да |
| `tools/call` | да |
| `ping` | да |
| stdio-транспорт (локальный процесс) | нет и не планируется: на Android нет дешёвого способа держать произвольный дочерний процесс |
| SSE-поток, `resources/*`, `prompts/*`, sampling | нет |
| OAuth-авторизация | нет, только статический токен в заголовке |

Если сервер требует SSE — он обычно всё равно принимает обычный `POST` с
`Accept: application/json`, этого достаточно.

## 2. Как подключить

«Инструменты → MCP-серверы»:

1. **Из каталога** — список преднастроенных серверов (`assets/mcp/servers.json`).
2. **Свой сервер** — URL + необязательный заголовок `Authorization`.
3. **Вручную (JSON)** — полный объект конфигурации, если нужно несколько
   заголовков.

Конфигурация сохраняется в `SharedPreferences` (ключ `mcp_servers`) как JSON-массив:

```json
[
  {
    "name": "files",
    "url": "https://mcp.example.com/mcp",
    "headers": {"Authorization": "Bearer <token>"},
    "enabled": true,
    "timeout_ms": 20000
  }
]
```

`enabled: false` оставляет сервер в списке, но не опрашивает его при старте — это
важно, потому что недоступный сервер добавляет задержку в открытие чата.

## 3. Жизненный цикл сессии

```
POST initialize                       -> {serverInfo, capabilities, protocolVersion}
POST notifications/initialized        -> (без ответа)
POST tools/list                       -> {tools: [{name, description, inputSchema}]}
POST tools/call {name, arguments}     -> {content: [{type:"text", text:"..."}], isError?}
```

* **`Mcp-Session-Id`** — если сервер вернул его в заголовке `initialize`,
  приложение сохраняет его и отправляет во всех последующих запросах. Без этого
  сервер отвечает `400`.
* **`protocolVersion`** — отправляется `2025-06-18`; если сервер отвечает другой
  версией, приложение принимает её и продолжает (мягкая деградация).
* **Кэш инструментов** — список кэшируется на время сессии, `tools/list`
  вызывается один раз при подключении. Кнопка «Обновить» в меню сбрасывает кэш.
* **Таймауты** — `timeout_ms` на запрос; при ошибке сервер помечается
  недоступным и его инструменты исключаются из промпта до следующего «Обновить».

## 4. Как результат попадает к модели

`tools/call` возвращает массив `content`. Приложение склеивает все блоки
`type: "text"` через `\n`, обрезает до 4 КБ и отдаёт в цикл агента как
`Observation`. `isError: true` не роняет шаг: модель видит
`Observation: ошибка инструмента <name>: <текст>` и пробует другой путь —
это часть политики «работает до успеха».

Если сервер вернул `content` с изображением или ресурсом, блок пропускается
(текстовая модель всё равно не сможет его прочитать), в наблюдение попадает
пометка `[non-text content omitted]`.

## 5. Локальный сервер для тестов

Самый быстрый способ проверить связку — поднять MCP-сервер на ноутбуке и
прокинуть порт в телефон:

```bash
adb reverse tcp:8931 tcp:8931
```

В приложении укажите `http://localhost:8931/mcp`. Именно `localhost` — остальные
`http://`-адреса заблокированы (см. `docs/PLUGINS.md`, раздел «Безопасность»).

Минимальный сервер на Python (только `initialize`, `tools/list`, `tools/call`):

```python
from http.server import BaseHTTPRequestHandler, HTTPServer
import json

class H(BaseHTTPRequestHandler):
    def do_POST(self):
        req = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        m, i = req["method"], req.get("id")
        if m == "initialize":
            res = {"protocolVersion": "2025-06-18",
                   "capabilities": {"tools": {}},
                   "serverInfo": {"name": "demo", "version": "0.1"}}
        elif m == "tools/list":
            res = {"tools": [{"name": "echo", "description": "Возвращает аргумент",
                              "inputSchema": {"type": "object",
                                              "properties": {"text": {"type": "string"}},
                                              "required": ["text"]}}]}
        elif m == "tools/call":
            res = {"content": [{"type": "text",
                                "text": req["params"]["arguments"].get("text", "")}]}
        else:
            res = {}
        body = json.dumps({"jsonrpc": "2.0", "id": i, "result": res}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Mcp-Session-Id", "demo-session")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

HTTPServer(("0.0.0.0", 8931), H).serve_forever()
```

Проверка из терминала:

```bash
curl -s http://localhost:8931/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | head -c 400
```

В чате спросите «повтори слово привет через echo» — в ответе должен появиться
чип `echo(...) → привет`.

## 6. Отладка

| Симптом | Причина / решение |
|---|---|
| `MCP 400` на первом запросе | не отправлен `Mcp-Session-Id` или неверный `Content-Type` |
| `MCP 401/403` | токен протух или не тот заголовок; проверьте `Authorization` |
| `MCP 404` | неверный путь: у многих серверов это `/mcp`, а не `/` |
| `пустой tools/list` | сервер отдаёт инструменты только после `notifications/initialized` |
| Инструменты есть, но модель их не вызывает | включите ReAct: слабые модели игнорируют длинный список в `auto` |
| Долгое открытие чата | отключите недоступные серверы (`enabled: false`) |

Логи: `adb logcat -s TurboLLM | grep -i mcp` — печатается метод, статус и размер
ответа, тела не логируются (там могут быть токены).
