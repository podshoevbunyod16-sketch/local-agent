package ai.turbollm.app;

import android.content.Context;
import android.content.SharedPreferences;
import android.os.Build;
import android.os.Environment;

import java.io.File;
import java.util.Arrays;
import java.util.Comparator;

/** App settings + GGUF model discovery. */
public final class Settings {

    private static final String PREFS = "turbollm";

    public final SharedPreferences prefs;

    public Settings(Context ctx) {
        prefs = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE);
    }

    public String modelPath() {
        return prefs.getString("model_path", "");
    }

    public void setModelPath(String path) {
        prefs.edit().putString("model_path", path).apply();
    }

    public int nCtx() {
        return prefs.getInt("n_ctx", 4096);
    }

    /** Quantised KV cache: 0=f16, 1=q8_0 (default), 2=q4_0. */
    public int kvType() {
        return prefs.getInt("kv_type", 1);
    }

    public int nGpuLayers() {
        return prefs.getInt("n_gpu_layers", 0);
    }

    public boolean flashAttn() {
        return prefs.getBoolean("flash_attn", true);
    }

    /** All threads minus one, clamped: big.LITTLE phones lose speed with all cores. */
    public int nThreads() {
        int cores = Runtime.getRuntime().availableProcessors();
        int def = Math.max(2, Math.min(6, cores - 1));
        return prefs.getInt("n_threads", def);
    }

    public int maxTokens() {
        return prefs.getInt("max_tokens", 512);
    }

    public float temperature() {
        return prefs.getFloat("temperature", 0.7f);
    }

    public int topK() {
        return prefs.getInt("top_k", 40);
    }

    public float topP() {
        return prefs.getFloat("top_p", 0.95f);
    }

    public float repeatPenalty() {
        return prefs.getFloat("repeat_penalty", 1.08f);
    }

    // ------------------------------------------------------------- agent

    /** Master switch: let the model call tools (built-in + plugins + MCP). */
    public boolean agentEnabled() {
        return prefs.getBoolean("agent_enabled", true);
    }

    /** Force tool calls to be valid JSON via a GBNF grammar (recommended). */
    public boolean forceJsonTools() {
        return prefs.getBoolean("agent_force_json", true);
    }

    /** ReAct loop (Thought/Action/Observation) instead of free tool calling. */
    public boolean reactEnabled() {
        return prefs.getBoolean("agent_react", false);
    }

    public int agentMaxSteps() {
        return prefs.getInt("agent_max_steps", 4);
    }

    /** "auto" | "qwen" | "llama3" | "json". */
    public String toolStyle() {
        return prefs.getString("tool_style", "auto");
    }

    public boolean useMcp() {
        return prefs.getBoolean("use_mcp", false);
    }

    /** Extra MCP servers typed in by the user: JSON array of ServerConfig. */
    public String manualMcpServers() {
        return prefs.getString("mcp_servers", "[]");
    }

    public void setManualMcpServers(String json) {
        prefs.edit().putString("mcp_servers", json).apply();
    }

    public String systemPrompt() {
        return prefs.getString("system_prompt",
                "Ты — полезный ассистент. Отвечай кратко и по делу.");
    }

    public void save(String key, int value) {
        prefs.edit().putInt(key, value).apply();
    }

    public void save(String key, float value) {
        prefs.edit().putFloat(key, value).apply();
    }

    public void save(String key, boolean value) {
        prefs.edit().putBoolean(key, value).apply();
    }

    public void save(String key, String value) {
        prefs.edit().putString(key, value).apply();
    }

    // ------------------------------------------------------------- model files

    /**
     * Directories where a GGUF file may live, most likely first. We never ask for
     * MANAGE_EXTERNAL_STORAGE: the app-private folder plus the public Download
     * folder are enough, and the latter is readable without permissions on
     * Android 10+ only through SAF — so we also offer the in-app picker.
     */
    public static File[] modelDirs(Context ctx) {
        return new File[] {
                new File(ctx.getExternalFilesDir(null), "models"),
                ctx.getFilesDir(),
                Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOWNLOADS),
                Environment.getExternalStorageDirectory(),
        };
    }

    /** Scans the known folders for *.gguf, newest/largest first. */
    public static File[] findModels(Context ctx) {
        java.util.LinkedHashMap<String, File> found = new java.util.LinkedHashMap<>();
        for (File dir : modelDirs(ctx)) {
            if (dir == null || !dir.isDirectory()) {
                continue;
            }
            File[] files = dir.listFiles();
            if (files == null) {
                continue;
            }
            Arrays.sort(files, Comparator.comparingLong(File::lastModified).reversed());
            for (File f : files) {
                if (f.isFile() && f.getName().toLowerCase().endsWith(".gguf")) {
                    found.put(f.getAbsolutePath(), f);
                }
            }
        }
        File[] out = found.values().toArray(new File[0]);
        Arrays.sort(out, Comparator.comparingLong(File::length).reversed());
        return out;
    }

    /** Recommended folder for downloaded models (app-private, no permissions). */
    public static File defaultModelDir(Context ctx) {
        File dir = new File(ctx.getExternalFilesDir(null), "models");
        //noinspection ResultOfMethodCallIgnored
        dir.mkdirs();
        return dir;
    }

    public static String deviceSummary() {
        return Build.MANUFACTURER + " " + Build.MODEL
                + " · " + Build.SUPPORTED_ABIS[0]
                + " · " + Runtime.getRuntime().availableProcessors() + " ядер";
    }
}
