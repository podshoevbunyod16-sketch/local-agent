package ai.turbollm.app;

import android.app.Activity;
import android.app.AlertDialog;
import android.content.Intent;
import android.graphics.drawable.GradientDrawable;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.text.InputType;
import android.util.Log;
import android.view.Gravity;
import android.view.View;
import android.widget.Button;
import android.widget.EditText;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.TextView;
import android.widget.Toast;

import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.InputStream;
import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Locale;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/**
 * Single-screen chat UI, written against the platform framework only (no
 * AndroidX, no Compose) so that the APK can be built with plain SDK tools –
 * aapt2 + javac + d8 + apksigner – which makes the GitHub Actions workflow both
 * fast and dependency-free. See scripts/build-apk.sh.
 */
public final class MainActivity extends Activity {

    private static final String TAG = "TurboLLM";
    private static final int REQ_PICK_GGUF = 1001;

    /**
     * v0.1 died silently when the native side crashed: the user saw the app just
     * close. A crash in the JNI layer cannot be caught from Java, but every Java
     * exception — and every Rust error we surface — now ends up in a dialog that
     * says what happened, and the log gets a clear tag for `adb logcat`.
     */
    private void installCrashHandler() {
        final Thread.UncaughtExceptionHandler previous = Thread.getDefaultUncaughtExceptionHandler();
        Thread.setDefaultUncaughtExceptionHandler((thread, error) -> {
            Log.e(TAG, "fatal in " + thread.getName(), error);
            try {
                String msg = error.getClass().getSimpleName() + ": "
                        + String.valueOf(error.getMessage());
                new AlertDialog.Builder(MainActivity.this)
                        .setTitle("TurboLLM упал")
                        .setMessage(msg + "\n\nПодробности: adb logcat -s TurboLLM")
                        .setPositiveButton("Закрыть", (d, w) -> {
                            if (previous != null) {
                                previous.uncaughtException(thread, error);
                            }
                        })
                        .show();
                return;
            } catch (Throwable ignored) {
                // fall through to the default handler
            }
            if (previous != null) {
                previous.uncaughtException(thread, error);
            }
        });
    }

    private final Handler ui = new Handler(Looper.getMainLooper());
    private final ExecutorService worker = Executors.newSingleThreadExecutor();
    private final List<NativeBridge.Msg> history = new ArrayList<>();

    private Settings settings;
    private NativeBridge.Session session;
    private ToolExecutor toolExecutor;
    private ScrollView scroll;
    private LinearLayout chatContainer;
    private EditText input;
    private Button sendButton;
    private TextView status;

    /** Currently streamed assistant message. */
    private TextView streamingView;
    private final StringBuilder streamingText = new StringBuilder();
    private boolean generating;
    private long genStartMs;
    private int genTokens;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        installCrashHandler();
        setContentView(R.layout.activity_main);
        settings = new Settings(this);

        scroll = findViewById(R.id.scroll);
        chatContainer = findViewById(R.id.chat_container);
        input = findViewById(R.id.input);
        sendButton = findViewById(R.id.btn_send);
        status = findViewById(R.id.tv_status);

        sendButton.setOnClickListener(v -> {
            if (generating) {
                cancelGeneration();
            } else {
                sendMessage();
            }
        });
        findViewById(R.id.btn_new_chat).setOnClickListener(v -> newChat());
        findViewById(R.id.btn_settings).setOnClickListener(v -> showSettings());
        findViewById(R.id.btn_tools).setOnClickListener(v -> showTools());
        android.widget.CheckBox react = findViewById(R.id.cb_react);
        react.setChecked(settings.reactEnabled());
        react.setOnCheckedChangeListener((v, checked) -> {
            settings.save("agent_react", checked);
            setStatus(checked
                    ? "ReAct-агент: Thought → Action → Observation до финального ответа"
                    : "Обычный режим: модель сама решает, когда вызывать инструменты");
        });
        toolExecutor = new ToolExecutor(() -> Settings.deviceSummary()
                + ", Android " + Build.VERSION.RELEASE
                + ", свободно " + (Runtime.getRuntime().freeMemory() / 1048576) + " МБ");
        findViewById(R.id.btn_model).setOnClickListener(v -> showModelPicker());

        updateStatus();

        // A .gguf opened from a file manager.
        Intent intent = getIntent();
        if (intent != null && Intent.ACTION_VIEW.equals(intent.getAction())
                && intent.getData() != null) {
            importModelUri(intent.getData());
        }

        // Restore the last model automatically.
        String path = settings.modelPath();
        if (path != null && !path.isEmpty() && new File(path).isFile()) {
            loadModelAsync(new File(path));
        } else {
            addBubble("assistant", getString(R.string.download_hint));
        }
    }

    // ------------------------------------------------------------------ model

    private void showModelPicker() {
        File[] found = Settings.findModels(this);
        String[] names = new String[found.length + 2];
        names[0] = "Выбрать файл .gguf вручную…";
        names[1] = "Указать путь вручную: " + Settings.defaultModelDir(this).getAbsolutePath();
        for (int i = 0; i < found.length; i++) {
            names[i + 2] = found[i].getName()
                    + "  (" + String.format(Locale.US, "%.0f МБ", found[i].length() / 1048576f) + ")";
        }
        new AlertDialog.Builder(this)
                .setTitle("Выберите GGUF-модель")
                .setItems(names, (d, which) -> {
                    if (which == 0) {
                        pickGgufFile();
                    } else if (which == 1) {
                        Toast.makeText(this, Settings.defaultModelDir(this).getAbsolutePath(),
                                Toast.LENGTH_LONG).show();
                    } else {
                        loadModelAsync(found[which - 2]);
                    }
                })
                .setNegativeButton("Отмена", null)
                .show();
    }

    private void pickGgufFile() {
        Intent i = new Intent(Intent.ACTION_OPEN_DOCUMENT);
        i.addCategory(Intent.CATEGORY_OPENABLE);
        i.setType("*/*");
        try {
            startActivityForResult(i, REQ_PICK_GGUF);
        } catch (Exception e) {
            Toast.makeText(this, "Нет файлового менеджера", Toast.LENGTH_SHORT).show();
        }
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode == REQ_PICK_GGUF && resultCode == RESULT_OK && data != null
                && data.getData() != null) {
            importModelUri(data.getData());
        }
    }

    /** Copies a content:// GGUF into app storage (llama.cpp needs a real path). */
    private void importModelUri(android.net.Uri uri) {
        setStatus("Копирование модели…");
        worker.execute(() -> {
            try (InputStream in = getContentResolver().openInputStream(uri)) {
                File out = new File(Settings.defaultModelDir(this), "model.gguf");
                try (FileOutputStream fos = new FileOutputStream(out)) {
                    byte[] buf = new byte[1 << 20];
                    int n;
                    while ((n = in.read(buf)) > 0) {
                        fos.write(buf, 0, n);
                    }
                }
                ui.post(() -> loadModelAsync(out));
            } catch (Exception e) {
                Log.e(TAG, "import failed", e);
                ui.post(() -> setStatus("Импорт не удался: " + e.getMessage()));
            }
        });
    }

    private void loadModelAsync(File file) {
        setStatus(getString(R.string.loading_model) + " " + file.getName());
        worker.execute(() -> {
            long t0 = System.currentTimeMillis();
            NativeBridge.Session s = NativeBridge.load(
                    file.getAbsolutePath(),
                    settings.nCtx(),
                    settings.nThreads(),
                    settings.nThreads(),
                    settings.nGpuLayers(),
                    settings.kvType(),
                    settings.flashAttn(),
                    settings.systemPrompt() + buildSystemPrompt());
            long dt = System.currentTimeMillis() - t0;
            ui.post(() -> {
                if (s == null) {
                    String err = NativeBridge.lastError();
                    setStatus("Ошибка загрузки: " + NativeBridge.jsonString(err, "message"));
                    addBubble("assistant", "Не удалось загрузить модель:\n"
                            + NativeBridge.jsonString(err, "message"));
                    return;
                }
                if (session != null) {
                    session.close();
                }
                session = s;
                settings.setModelPath(file.getAbsolutePath());
                history.clear();
                newChat();
                String info = s.modelInfoJson();
                String model = NativeBridge.jsonString(info, "description");
                setStatus("Модель: " + (model == null ? file.getName() : model)
                        + " · ctx " + settings.nCtx()
                        + " · " + settings.nThreads() + " потоков"
                        + " · загружена за " + (dt / 1000.0) + " с");
            });
        });
    }

    // ------------------------------------------------------------------- chat

    private void sendMessage() {
        if (session == null) {
            showModelPicker();
            return;
        }
        String text = input.getText().toString().trim();
        if (text.isEmpty()) {
            return;
        }
        input.setText("");
        addBubble("user", text);
        history.add(new NativeBridge.Msg("user", text));

        startGeneration();
    }

    private void startGeneration() {
        if (settings.agentEnabled()) {
            startAgent();
            return;
        }
        generating = true;
        genStartMs = System.currentTimeMillis();
        genTokens = 0;
        sendButton.setText(R.string.btn_stop);
        streamingText.setLength(0);
        streamingView = addBubble("assistant", "");

        final String messagesJson = NativeBridge.messagesJson(history);
        final String paramsJson = NativeBridge.paramsJson(
                settings.maxTokens(), settings.temperature(), settings.topK(),
                settings.topP(), settings.repeatPenalty(), settings.nThreads(), true);
        final StringBuilder acc = new StringBuilder();

        worker.execute(() -> {
            String result = session.generate(messagesJson, paramsJson, piece -> {
                acc.append(piece);
                genTokens++;
                ui.post(() -> {
                    streamingText.append(piece);
                    if (streamingView != null) {
                        streamingView.setText(streamingText.toString());
                        scroll.fullScroll(View.FOCUS_DOWN);
                    }
                });
            });
            ui.post(() -> finishGeneration(result, acc.toString()));
        });
    }

    private void finishGeneration(String result, String text) {
        generating = false;
        sendButton.setText(R.string.btn_send);
        long dt = System.currentTimeMillis() - genStartMs;

        if (result == null) {
            String err = NativeBridge.lastError();
            String code = NativeBridge.jsonString(err, "code");
            String msg = NativeBridge.jsonString(err, "message");
            if (streamingView != null && "CANCELLED".equals(code)) {
                streamingView.setText(text + "\n\n⏹ остановлено");
            } else if (streamingView != null) {
                streamingView.setText(text + "\n\n⚠ " + msg);
            }
            if (text != null && !text.isEmpty()) {
                history.add(new NativeBridge.Msg("assistant", text));
            }
            return;
        }

        String statsJson = result;
        double tps = NativeBridge.jsonNumber(statsJson, "decode_tps", 0);
        double prefillTps = NativeBridge.jsonNumber(statsJson, "prefill_tps", 0);
        double cached = NativeBridge.jsonNumber(statsJson, "cached_tokens", 0);
        double promptTokens = NativeBridge.jsonNumber(statsJson, "prompt_tokens", 0);
        double ttft = NativeBridge.jsonNumber(statsJson, "ttft_ms", dt);

        if (text.isEmpty()) {
            text = "(пустой ответ)";
            if (streamingView != null) {
                streamingView.setText(text);
            }
        }
        history.add(new NativeBridge.Msg("assistant", text));

        // The badge that shows the KV-cache win: "cached 1240 of 1320 tokens".
        String badge = String.format(Locale.US,
                "%.1f tok/s · TTFT %.0f мс · из кэша %.0f / %.0f токенов · prefill %.0f tok/s",
                tps, ttft, cached, cached + promptTokens, prefillTps);
        TextView tv = new TextView(this);
        tv.setText(badge);
        tv.setTextSize(11f);
        tv.setTextColor(getColor(R.color.muted));
        tv.setPadding(dp(12), 0, dp(12), dp(4));
        chatContainer.addView(tv);
        scroll.fullScroll(View.FOCUS_DOWN);
        updateStatus();
    }

    // ------------------------------------------------------------------ agent

    /** Collect built-in + plugin + MCP tools, then run the agent loop. */
    private void startAgent() {
        generating = true;
        genStartMs = System.currentTimeMillis();
        genTokens = 0;
        sendButton.setText(R.string.btn_stop);
        streamingText.setLength(0);
        streamingView = null;

        final String messagesJson = NativeBridge.messagesJson(history);

        worker.execute(() -> {
            String tools = collectTools();
            String config = NativeBridge.agentConfigJson(
                    settings.agentMaxSteps(),
                    settings.forceJsonTools(),
                    settings.toolStyle(),
                    settings.maxTokens(),
                    settings.temperature(),
                    settings.nThreads(),
                    settings.reactEnabled(),
                    2);

            final StringBuilder acc = new StringBuilder();
            final StringBuilder visible = new StringBuilder();
            final boolean[] toolUsed = {false};

            String result = session.runAgent(messagesJson, tools, config,
                    json -> ui.post(() -> onAgentEvent(json, visible, toolUsed)),
                    // Executor: called for HTTP / Android / MCP tools.
                    (name, argsJson) -> toolExecutor.execute(name, argsJson));

            ui.post(() -> {
                generating = false;
                sendButton.setText(R.string.btn_send);
                if (result == null) {
                    String err = NativeBridge.lastError();
                    String code = NativeBridge.jsonString(err, "code");
                    String msg = NativeBridge.jsonString(err, "message");
                    String shown = visible.length() > 0 ? visible.toString() : "";
                    if ("CANCELLED".equals(code)) {
                        addBubble("assistant", shown + "\n\n⏹ остановлено");
                    } else {
                        addBubble("assistant", shown + "\n\n⚠ " + msg
                                + "\n\nКод: " + code
                                + "\nЕсли ошибка повторяется — попробуйте другую модель "
                                + "или выключите ReAct/инструменты в меню «Инструменты».");
                        Log.e(TAG, "agent failed: " + err);
                    }
                    if (!shown.isEmpty()) {
                        history.add(new NativeBridge.Msg("assistant", shown));
                    }
                    return;
                }
                String text = NativeBridge.jsonString(result, "text");
                if (text == null || text.isEmpty()) {
                    text = visible.toString();
                }
                if (text.isEmpty()) {
                    text = "(пустой ответ)";
                }
                if (visible.length() == 0) {
                    addBubble("assistant", text);
                }
                history.add(new NativeBridge.Msg("assistant", text));
                double tps = NativeBridge.jsonNumber(result, "decode_tps", 0);
                double cached = NativeBridge.jsonNumber(result, "cached_tokens", 0);
                double prompt = NativeBridge.jsonNumber(result, "prompt_tokens", 0);
                addStatsLine(String.format(Locale.US,
                        "%.1f tok/s · из кэша %.0f / %.0f токенов · инструменты: %s",
                        tps, cached, cached + prompt,
                        toolUsed[0] ? "да" : "нет"));
            });
        });
    }

    /** Handles one event from the Rust agent loop. */
    private void onAgentEvent(String json, StringBuilder visible, boolean[] toolUsed) {
        try {
            JSONObject ev = new JSONObject(json);
            String type = ev.optString("type");
            switch (type) {
                case "token": {
                    String t = ev.optString("text", "");
                    if (t.isEmpty()) {
                        break;
                    }
                    visible.append(t);
                    if (streamingView == null) {
                        streamingView = addBubble("assistant", "");
                    }
                    streamingText.append(t);
                    streamingView.setText(streamingText.toString());
                    scroll.fullScroll(View.FOCUS_DOWN);
                    break;
                }
                case "thought": {
                    String t = ev.optString("text", "").trim();
                    int step = ev.optInt("step", 0);
                    if (!t.isEmpty()) {
                        addToolChip("💭 шаг " + step + ": " + shorten(t, 300), R.color.on_surface);
                    }
                    streamingText.setLength(0);
                    streamingView = null;
                    break;
                }
                case "tool_call": {
                    toolUsed[0] = true;
                    String name = ev.optString("name");
                    String args = ev.optJSONObject("arguments") == null
                            ? "{}" : ev.optJSONObject("arguments").toString();
                    addToolChip("🔧 " + name + " " + shorten(args, 160), R.color.accent);
                    // Anything streamed before the call is "thinking out loud".
                    streamingText.setLength(0);
                    streamingView = null;
                    break;
                }
                case "tool_result": {
                    boolean ok = ev.optBoolean("ok", true);
                    String out = shorten(ev.optString("output", ""), 400);
                    addToolChip((ok ? "✅ " : "⚠ ") + out,
                            ok ? R.color.muted : 0xFFFF6B6B);
                    streamingText.setLength(0);
                    streamingView = null;
                    break;
                }
                case "done": {
                    JSONObject stats = ev.optJSONObject("stats");
                    if (stats != null) {
                        genTokens = stats.optInt("generated_tokens", genTokens);
                    }
                    break;
                }
                default:
                    break;
            }
        } catch (JSONException e) {
            Log.w(TAG, "bad agent event: " + json, e);
        }
    }

    private static String shorten(String s, int n) {
        return s.length() <= n ? s : s.substring(0, n) + "…";
    }

    /** Small monospace line that shows what the agent did. */
    private void addToolChip(String text, int colorRes) {
        TextView tv = new TextView(this);
        tv.setText(text);
        tv.setTextSize(12f);
        tv.setTypeface(android.graphics.Typeface.MONOSPACE);
        tv.setTextColor(colorRes == 0xFFFF6B6B ? colorRes : getColor(colorRes));
        tv.setBackgroundResource(R.drawable.bubble_bot);
        tv.setPadding(dp(10), dp(6), dp(10), dp(6));
        LinearLayout.LayoutParams lp = new LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT);
        lp.bottomMargin = dp(4);
        tv.setLayoutParams(lp);
        chatContainer.addView(tv);
        scroll.post(() -> scroll.fullScroll(View.FOCUS_DOWN));
    }

    private void addStatsLine(String text) {
        TextView tv = new TextView(this);
        tv.setText(text);
        tv.setTextSize(11f);
        tv.setTextColor(getColor(R.color.muted));
        tv.setPadding(dp(12), 0, dp(12), dp(4));
        chatContainer.addView(tv);
        scroll.post(() -> scroll.fullScroll(View.FOCUS_DOWN));
    }

    /** Build the tool list: built-ins + enabled plugins + MCP servers. */
    private String collectTools() {
        List<String> parts = new ArrayList<>();
        parts.add(NativeBridge.nativeBuiltinTools());
        toolExecutor.clear();
        for (JSONObject plugin : PluginTools.installed(this)) {
            if (!plugin.optBoolean("enabled", true)) {
                continue;
            }
            String promptJson = NativeBridge.nativePluginPrompt(plugin.toString());
            try {
                JSONObject p = new JSONObject(promptJson == null ? "{}" : promptJson);
                if (p.optBoolean("ok", false)) {
                    parts.add(p.optJSONArray("tools") == null
                            ? "[]" : p.optJSONArray("tools").toString());
                }
            } catch (JSONException ignored) {
                // skip broken plugin
            }
            PluginTools.registerHttpTools(plugin, toolExecutor);
            if (settings.useMcp()) {
                for (PluginTools.McpSpec spec : PluginTools.mcpServers(plugin)) {
                    toolExecutor.registerMcpServer(spec.name, spec);
                }
            }
        }
        if (settings.useMcp()) {
            registerManualMcpServers();
            String mcpTools = toolExecutor.listMcpTools();
            if (mcpTools != null && !mcpTools.isEmpty()) {
                parts.add(mcpTools);
            }
        }
        return ToolExecutor.mergeTools(parts);
    }

    private void registerManualMcpServers() {
        try {
            JSONArray arr = new JSONArray(settings.manualMcpServers());
            for (int i = 0; i < arr.length(); i++) {
                JSONObject o = arr.optJSONObject(i);
                if (o != null && o.optBoolean("enabled", true)) {
                    toolExecutor.registerMcpServer(o.optString("name", "mcp_" + i),
                            PluginTools.McpSpec.fromJson(o));
                }
            }
        } catch (JSONException e) {
            Log.w(TAG, "bad mcp_servers setting", e);
        }
    }

    /** System prompt = base prompt + plugin / skill instructions. */
    private String buildSystemPrompt() {
        StringBuilder sb = new StringBuilder();
        for (JSONObject plugin : PluginTools.installed(this)) {
            if (!plugin.optBoolean("enabled", true)) {
                continue;
            }
            JSONObject m = plugin.optJSONObject("manifest");
            if (m != null && !m.optString("system_prompt", "").isEmpty()) {
                sb.append(m.optString("system_prompt")).append('\n');
            }
            String promptJson = NativeBridge.nativePluginPrompt(plugin.toString());
            try {
                JSONObject p = new JSONObject(promptJson == null ? "{}" : promptJson);
                sb.append(p.optString("prompt", ""));
            } catch (JSONException ignored) {
                // ignore
            }
        }
        return sb.toString();
    }

    // --------------------------------------------------------------- tools UI

    private void showTools() {
        String[] items = {
                (settings.agentEnabled() ? "✓ " : "  ") + "Агент и инструменты (tool calling)",
                (settings.forceJsonTools() ? "✓ " : "  ") + "Жёсткий JSON для вызовов (GBNF)",
                (settings.useMcp() ? "✓ " : "  ") + "MCP-серверы включены",
                "Лимит шагов агента: " + settings.agentMaxSteps(),
                "Синтаксис вызовов: " + settings.toolStyle(),
                "Установить плагин из GitHub…",
                "Добавить MCP-сервер вручную…",
                "Установленные плагины (" + PluginTools.installed(this).size() + ")",
                "Встроенные инструменты: now, calc, device_info",
        };
        new AlertDialog.Builder(this)
                .setTitle("Инструменты и плагины")
                .setItems(items, (d, w) -> {
                    switch (w) {
                        case 0:
                            settings.save("agent_enabled", !settings.agentEnabled());
                            showTools();
                            break;
                        case 1:
                            settings.save("agent_force_json", !settings.forceJsonTools());
                            showTools();
                            break;
                        case 2:
                            settings.save("use_mcp", !settings.useMcp());
                            showTools();
                            break;
                        case 3:
                            pickNumber("Лимит шагов агента", settings.agentMaxSteps(), 1, 8,
                                    v -> settings.save("agent_max_steps", v));
                            break;
                        case 4:
                            new AlertDialog.Builder(this)
                                    .setTitle("Синтаксис вызовов")
                                    .setSingleChoiceItems(
                                            new String[] {"auto", "qwen", "llama3", "json"},
                                            Arrays.asList("auto", "qwen", "llama3", "json")
                                                    .indexOf(settings.toolStyle()),
                                            (dd, ww) -> {
                                                settings.save("tool_style",
                                                        new String[] {"auto", "qwen", "llama3",
                                                                "json"}[ww]);
                                                dd.dismiss();
                                            })
                                    .show();
                            break;
                        case 5:
                            installPluginDialog();
                            break;
                        case 6:
                            addMcpServerDialog();
                            break;
                        case 7:
                            showInstalled();
                            break;
                        default:
                            break;
                    }
                })
                .setNegativeButton("Закрыть", null)
                .show();
    }

    private interface IntConsumer {
        void accept(int v);
    }

    private void pickNumber(String title, int current, int min, int max, IntConsumer cb) {
        EditText et = new EditText(this);
        et.setInputType(InputType.TYPE_CLASS_NUMBER);
        et.setText(String.valueOf(current));
        new AlertDialog.Builder(this)
                .setTitle(title)
                .setView(et)
                .setPositiveButton("OK", (d, w) -> {
                    try {
                        cb.accept(Math.max(min, Math.min(max,
                                Integer.parseInt(et.getText().toString().trim()))));
                    } catch (NumberFormatException ignored) {
                        // keep current value
                    }
                })
                .setNegativeButton("Отмена", null)
                .show();
    }

    private void installPluginDialog() {
        EditText et = new EditText(this);
        et.setHint("owner/repo или https://github.com/owner/repo");
        et.setText("anthropics/claude-code");
        new AlertDialog.Builder(this)
                .setTitle("Установить плагин из GitHub")
                .setMessage("Ищет turbollm.plugin.json (или .claude-plugin/plugin.json "
                        + "в стиле Claude Code) и ставит плагин локально.")
                .setView(et)
                .setPositiveButton("Установить", (d, w) -> {
                    String src = et.getText().toString().trim();
                    if (src.isEmpty()) {
                        return;
                    }
                    setStatus("Установка плагина…");
                    worker.execute(() -> {
                        PluginTools.Result r = PluginTools.install(this, src);
                        ui.post(() -> {
                            setStatus(r.message);
                            new AlertDialog.Builder(this)
                                    .setTitle(r.ok ? "Готово" : "Не удалось")
                                    .setMessage(r.message)
                                    .setPositiveButton("OK", null)
                                    .show();
                        });
                    });
                })
                .setNegativeButton("Отмена", null)
                .show();
    }

    private void addMcpServerDialog() {
        LinearLayout box = new LinearLayout(this);
        box.setOrientation(LinearLayout.VERTICAL);
        box.setPadding(dp(20), dp(8), dp(20), dp(8));
        EditText name = textField(box, "Имя", "my_server");
        EditText url = textField(box, "URL (https://… для HTTP-транспорта)",
                "https://example.com/mcp");
        EditText cmd = textField(box, "…или команда (stdio)", "");
        new AlertDialog.Builder(this)
                .setTitle("MCP-сервер")
                .setView(box)
                .setPositiveButton("Добавить", (d, w) -> {
                    try {
                        JSONObject o = new JSONObject();
                        o.put("name", name.getText().toString().trim());
                        String u = url.getText().toString().trim();
                        String c = cmd.getText().toString().trim();
                        if (!u.isEmpty()) {
                            o.put("url", u);
                        }
                        if (!c.isEmpty()) {
                            o.put("command", c);
                        }
                        o.put("enabled", true);
                        JSONArray arr = new JSONArray(settings.manualMcpServers());
                        arr.put(o);
                        settings.setManualMcpServers(arr.toString());
                        settings.save("use_mcp", true);
                        setStatus("MCP-сервер добавлен");
                    } catch (JSONException e) {
                        setStatus("Ошибка: " + e.getMessage());
                    }
                })
                .setNegativeButton("Отмена", null)
                .show();
    }

    private EditText textField(LinearLayout parent, String label, String value) {
        TextView tv = new TextView(this);
        tv.setText(label);
        tv.setTextSize(13f);
        parent.addView(tv);
        EditText et = new EditText(this);
        et.setText(value);
        parent.addView(et);
        return et;
    }

    private void showInstalled() {
        List<JSONObject> list = PluginTools.installed(this);
        if (list.isEmpty()) {
            setStatus("Плагинов нет");
            return;
        }
        String[] names = new String[list.size()];
        for (int i = 0; i < list.size(); i++) {
            JSONObject m = list.get(i).optJSONObject("manifest");
            String n = m == null ? "?" : m.optString("name", "?");
            boolean on = list.get(i).optBoolean("enabled", true);
            names[i] = (on ? "✓ " : "  ") + n + " — " + list.get(i).optString("source", "");
        }
        new AlertDialog.Builder(this)
                .setTitle("Установленные плагины")
                .setItems(names, (d, w) -> {
                    JSONObject plugin = list.get(w);
                    JSONObject m = plugin.optJSONObject("manifest");
                    String n = m == null ? "?" : m.optString("name", "?");
                    new AlertDialog.Builder(this)
                            .setTitle(n)
                            .setMessage(plugin.optString("source", "") + "\n\n"
                                    + (m == null ? "" : m.optString("description", "")))
                            .setPositiveButton(plugin.optBoolean("enabled", true)
                                            ? "Отключить" : "Включить",
                                    (dd, ww) -> {
                                        PluginTools.setEnabled(this, n,
                                                !plugin.optBoolean("enabled", true));
                                        showInstalled();
                                    })
                            .setNeutralButton("Удалить", (dd, ww) -> {
                                PluginTools.uninstall(this, n);
                                showInstalled();
                            })
                            .setNegativeButton("Закрыть", null)
                            .show();
                })
                .show();
    }

    private void cancelGeneration() {
        if (session != null) {
            session.cancel();
        }
    }

    private void newChat() {
        history.clear();
        chatContainer.removeAllViews();
        streamingText.setLength(0);
        if (session != null) {
            worker.execute(session::reset);
        }
        updateStatus();
    }

    private TextView addBubble(String role, String text) {
        View view = getLayoutInflater().inflate(R.layout.item_message, chatContainer, false);
        TextView tvRole = view.findViewById(R.id.tv_role);
        TextView tvText = view.findViewById(R.id.tv_text);
        boolean user = "user".equals(role);
        tvRole.setText(user ? "вы" : "ассистент");
        tvText.setText(text);
        GradientDrawable bg = (GradientDrawable) getResources()
                .getDrawable(user ? R.drawable.bubble_user : R.drawable.bubble_bot, null);
        tvText.setBackground(bg);
        LinearLayout.LayoutParams lp = new LinearLayout.LayoutParams(
                user ? (int) (getResources().getDisplayMetrics().widthPixels * 0.86f)
                        : LinearLayout.LayoutParams.MATCH_PARENT,
                LinearLayout.LayoutParams.WRAP_CONTENT);
        if (user) {
            lp.gravity = Gravity.END;
        }
        view.setLayoutParams(lp);
        chatContainer.addView(view);
        scroll.post(() -> scroll.fullScroll(View.FOCUS_DOWN));
        return tvText;
    }

    private int dp(int v) {
        return Math.round(v * getResources().getDisplayMetrics().density);
    }

    // --------------------------------------------------------------- settings

    private void showSettings() {
        LinearLayout box = new LinearLayout(this);
        box.setOrientation(LinearLayout.VERTICAL);
        box.setPadding(dp(20), dp(8), dp(20), dp(8));

        EditText nCtx = numberField(box, "Контекст (токенов)", settings.nCtx(), 512, 32768);
        EditText threads = numberField(box, "Потоков CPU", settings.nThreads(), 1, 16);
        EditText maxTok = numberField(box, "Лимит ответа (токенов)", settings.maxTokens(), 32, 8192);
        EditText gpu = numberField(box, "Слоёв на GPU (0 = CPU)",
                settings.nGpuLayers(), 0, 200);

        final String[] kvNames = {"f16 (макс. качество)", "q8_0 (быстрее, 0.6x памяти)",
                "q4_0 (самый быстрый, 0.35x памяти)"};
        final int[] kvChoice = {settings.kvType()};
        Button kv = new Button(this);
        kv.setAllCaps(false);
        kv.setText("KV-кэш: " + kvNames[Math.min(kvChoice[0], 2)]);
        kv.setOnClickListener(v -> new AlertDialog.Builder(this)
                .setTitle("Квантование KV-кэша")
                .setSingleChoiceItems(kvNames, Math.min(kvChoice[0], 2), (d, w) -> {
                    kvChoice[0] = w;
                    kv.setText("KV-кэш: " + kvNames[w]);
                    d.dismiss();
                })
                .show());
        box.addView(kv);

        final boolean[] flash = {settings.flashAttn()};
        Button flashBtn = new Button(this);
        flashBtn.setAllCaps(false);
        flashBtn.setText("Flash attention: " + (flash[0] ? "вкл" : "выкл"));
        flashBtn.setOnClickListener(v -> {
            flash[0] = !flash[0];
            flashBtn.setText("Flash attention: " + (flash[0] ? "вкл" : "выкл"));
        });
        box.addView(flashBtn);

        Button bench = new Button(this);
        bench.setText("Запустить бенчмарк");
        bench.setAllCaps(false);
        bench.setOnClickListener(v -> runBenchmark());
        box.addView(bench);

        ScrollView sv = new ScrollView(this);
        sv.addView(box);

        new AlertDialog.Builder(this)
                .setTitle("Настройки")
                .setView(sv)
                .setPositiveButton("Сохранить и перезагрузить", (d, w) -> {
                    settings.save("n_ctx", parseInt(nCtx, 4096, 512, 32768));
                    settings.save("n_threads", parseInt(threads, 4, 1, 16));
                    settings.save("max_tokens", parseInt(maxTok, 512, 32, 8192));
                    settings.save("n_gpu_layers", parseInt(gpu, 0, 0, 200));
                    settings.save("kv_type", kvChoice[0]);
                    settings.save("flash_attn", flash[0]);
                    String path = settings.modelPath();
                    if (path != null && !path.isEmpty()) {
                        loadModelAsync(new File(path));
                    }
                })
                .setNegativeButton("Отмена", null)
                .show();
    }

    private EditText numberField(LinearLayout parent, String label, int value, int min, int max) {
        TextView tv = new TextView(this);
        tv.setText(label);
        tv.setTextSize(13f);
        parent.addView(tv);
        EditText et = new EditText(this);
        et.setInputType(InputType.TYPE_CLASS_NUMBER);
        et.setText(String.valueOf(value));
        et.setTag(new int[] {min, max});
        parent.addView(et);
        return et;
    }

    private static int parseInt(EditText et, int def, int min, int max) {
        try {
            int v = Integer.parseInt(et.getText().toString().trim());
            return Math.max(min, Math.min(max, v));
        } catch (Exception e) {
            return def;
        }
    }

    private void runBenchmark() {
        if (session == null) {
            Toast.makeText(this, "Сначала загрузите модель", Toast.LENGTH_SHORT).show();
            return;
        }
        setStatus("Бенчмарк… (генерация 64 токена)");
        worker.execute(() -> {
            long t0 = System.currentTimeMillis();
            String json = session.bench("Расскажи в двух предложениях, как работает KV-кэш.", 64);
            long dt = System.currentTimeMillis() - t0;
            ui.post(() -> {
                if (json == null) {
                    setStatus("Бенчмарк не удался: "
                            + NativeBridge.jsonString(NativeBridge.lastError(), "message"));
                    return;
                }
                double tps = NativeBridge.jsonNumber(json, "decode_tps", 0);
                double prefill = NativeBridge.jsonNumber(json, "prefill_tps", 0);
                addBubble("assistant", String.format(Locale.US,
                        "Бенчмарк: %.1f ток/с генерация, %.0f ток/с prefill (%.1f с всего).",
                        tps, prefill, dt / 1000.0));
                setStatus(String.format(Locale.US, "%.1f tok/s · prefill %.0f tok/s", tps, prefill));
            });
        });
    }

    // ------------------------------------------------------------------ misc

    private void setStatus(String s) {
        status.setText(s);
    }

    private void updateStatus() {
        if (session == null) {
            setStatus(getString(R.string.no_model) + " · " + Settings.deviceSummary());
        } else {
            String t = status.getText().toString();
            if (t.startsWith(getString(R.string.no_model))) {
                setStatus("Модель загружена");
            }
        }
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        if (intent != null && Intent.ACTION_VIEW.equals(intent.getAction())
                && intent.getData() != null) {
            importModelUri(intent.getData());
        }
    }

    @Override
    protected void onDestroy() {
        final NativeBridge.Session s = session;
        session = null;
        if (s != null) {
            // Ask the engine to stop at the next token boundary, then wait for the
            // worker thread to unwind before freeing the native handles: closing
            // them while llama.cpp is still decoding would be a use-after-free.
            s.cancel();
            // NOTE: do NOT submit another task here — after shutdown() the pool
            // rejects it with RejectedExecutionException, which used to kill the
            // app exactly when the user left the screen.
            worker.shutdown();
            try {
                if (!worker.awaitTermination(3, java.util.concurrent.TimeUnit.SECONDS)) {
                    // The engine is still busy; leaked handles are better than a
                    // use-after-free, and the process is going away anyway.
                    Log.w(TAG, "generation did not stop in 3s, keeping native handles");
                    return;
                }
            } catch (InterruptedException ignored) {
                Thread.currentThread().interrupt();
            }
            try {
                s.close();
            } catch (Throwable t) {
                Log.w(TAG, "close failed", t);
            }
        }
        super.onDestroy();
    }
}
