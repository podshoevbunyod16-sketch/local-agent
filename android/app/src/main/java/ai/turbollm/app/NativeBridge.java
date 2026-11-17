package ai.turbollm.app;

/**
 * Thin JNI facade over the Rust core (crates/turbollm-core/src/jni_bridge.rs).
 *
 * <p>Every handle is an opaque {@code long} that points to a Box'ed Rust object.
 * Anything that can fail returns 0 / null and leaves a JSON error behind, which
 * {@link #lastError()} returns:
 *
 * <pre>{@code {"code":"MODEL_NOT_FOUND","message":"..."}}</pre>
 *
 * <p>There is no JSON library in the APK on purpose (keeps it tiny and lets us
 * build without Gradle/AndroidX, see scripts/build-apk.sh); this class contains
 * the small amount of parsing the UI needs.
 */
public final class NativeBridge {

    private NativeBridge() {}

    static {
        System.loadLibrary("turbollm_core");
    }

    // ---------------------------------------------------------------- native

    static native void nativeInit();

    /** JSON: {"gpu_offload":bool,"mmap":bool,"cpu_cores":int,"threads_default":int} */
    static native String nativeBackendInfo();

    static native long nativeLoad(String path, int nCtx, int nThreads, int nThreadsBatch,
                                  int nGpuLayers, int kvType, boolean flashAttn);

    static native void nativeFree(long engine);

    static native String nativeModelInfo(long engine);

    static native void nativeCancel(long engine);

    static native long nativeNewSession(long engine, String systemPrompt);

    static native void nativeFreeSession(long session);

    static native void nativeResetSession(long session, long engine);

    /** Returns JSON {"text":..., "stats":{...}} or null on error. */
    static native String nativeGenerate(long session, long engine, String messagesJson,
                                        String paramsJson, TokenSink sink);

    static native String nativeGenerateBench(long engine, String prompt, int maxTokens);

    static native String nativeLastError();

    // ----- v0.2: tools / MCP / plugins ------------------------------------

    /** JSON array of built-in tool specs (`now`, `calc`, `device_info`). */
    static native String nativeBuiltinTools();

    /**
     * Agent loop with tool calling.
     *
     * @return JSON {"text":…,"stats":{…}} or null (see {@link #lastError()}).
     */
    static native String nativeRunAgent(long session, long engine, String messagesJson,
                                        String toolsJson, String configJson,
                                        AgentSink sink, AgentExec executor);

    static native String nativePluginCheck(String manifestJson);

    static native String nativePluginSources(String source);

    static native String nativeStripSkill(String markdown);

    static native String nativePluginPrompt(String installedPluginJson);

    static native String nativeMcpInitialize(String clientVersion);

    static native String nativeMcpInitialized();

    static native String nativeMcpRequest(long id, String method, String paramsJson);

    static native String nativeMcpParse(String line);

    static native String nativeMcpTools(String server, String resultJson);

    static native String nativeMcpResult(String resultJson);

    static native String nativeDetectToolStyle(long engine);

    static native String nativeToolResponsePrompt(String output, int limit);

    /** Receives agent events as JSON: {"type":"token"|"tool_call"|"tool_result"|"done",…}. */
    public interface AgentSink {
        void onEvent(String json);
    }

    /** Executes a tool the core cannot: HTTP / Android / MCP. */
    public interface AgentExec {
        String execute(String name, String argsJson);
    }

    static native double nativeCachedHits(long session);

    /** Callback interface implemented on the Java side. */
    public interface TokenSink {
        void onToken(String piece);
    }

    // ------------------------------------------------------------- public API

    /** Loaded native engine + session, with Kotlin-style convenience wrappers. */
    public static final class Session {
        public final long engine;
        public final long session;

        Session(long engine, long session) {
            this.engine = engine;
            this.session = session;
        }

        public String modelInfoJson() {
            return nativeModelInfo(engine);
        }

        public void cancel() {
            nativeCancel(engine);
        }

        public void reset() {
            nativeResetSession(session, engine);
        }

        public double cachedHits() {
            return nativeCachedHits(session);
        }

        /** @return JSON {"text","stats"} or null (see {@link #lastError()}). */
        public String generate(String messagesJson, String paramsJson, TokenSink sink) {
            return nativeGenerate(session, engine, messagesJson, paramsJson, sink);
        }

        /** Agent loop: model ↔ tools until a final answer. */
        public String runAgent(String messagesJson, String toolsJson, String configJson,
                              AgentSink sink, AgentExec exec) {
            return nativeRunAgent(session, engine, messagesJson, toolsJson, configJson,
                    sink, exec);
        }

        public String toolStyle() {
            return nativeDetectToolStyle(engine);
        }

        public String bench(String prompt, int maxTokens) {
            return nativeGenerateBench(engine, prompt, maxTokens);
        }

        public void close() {
            nativeFreeSession(session);
            nativeFree(engine);
        }
    }

    /** @param kvType 0=f16, 1=q8_0, 2=q4_0 */
    public static Session load(String path, int nCtx, int nThreads, int nThreadsBatch,
                               int nGpuLayers, int kvType, boolean flashAttn,
                               String systemPrompt) {
        long engine = nativeLoad(path, nCtx, nThreads, nThreadsBatch, nGpuLayers, kvType,
                flashAttn);
        if (engine == 0L) {
            return null;
        }
        long session = nativeNewSession(engine, systemPrompt);
        if (session == 0L) {
            nativeFree(engine);
            return null;
        }
        return new Session(engine, session);
    }

    public static String lastError() {
        String e = nativeLastError();
        return e == null || e.isEmpty() ? "{\"code\":\"ENGINE\",\"message\":\"unknown error\"}" : e;
    }

    // ------------------------------------------------------------- tiny JSON

    /**
     * Extracts a string field from a flat JSON object. Enough for our own
     * payloads ({"code":...,"message":...} and {"text":...}).
     */
    static String jsonString(String json, String field) {
        if (json == null) {
            return null;
        }
        String needle = "\"" + field + "\":";
        int i = json.indexOf(needle);
        if (i < 0) {
            return null;
        }
        i += needle.length();
        while (i < json.length() && Character.isWhitespace(json.charAt(i))) {
            i++;
        }
        if (i >= json.length() || json.charAt(i) != '"') {
            return null;
        }
        StringBuilder sb = new StringBuilder();
        i++;
        while (i < json.length()) {
            char c = json.charAt(i++);
            if (c == '\\' && i < json.length()) {
                char n = json.charAt(i++);
                switch (n) {
                    case 'n': sb.append('\n'); break;
                    case 't': sb.append('\t'); break;
                    case 'r': sb.append('\r'); break;
                    case 'b': sb.append('\b'); break;
                    case 'f': sb.append('\f'); break;
                    case 'u':
                        if (i + 4 <= json.length()) {
                            sb.append((char) Integer.parseInt(json.substring(i, i + 4), 16));
                            i += 4;
                        }
                        break;
                    default: sb.append(n);
                }
            } else if (c == '"') {
                break;
            } else {
                sb.append(c);
            }
        }
        return sb.toString();
    }

    /** Extracts a numeric field (used for the stats block). */
    static double jsonNumber(String json, String field, double fallback) {
        if (json == null) {
            return fallback;
        }
        String needle = "\"" + field + "\":";
        int i = json.indexOf(needle);
        if (i < 0) {
            return fallback;
        }
        i += needle.length();
        int start = i;
        while (i < json.length() && "+-.0123456789eE".indexOf(json.charAt(i)) >= 0) {
            i++;
        }
        try {
            return Double.parseDouble(json.substring(start, i));
        } catch (NumberFormatException e) {
            return fallback;
        }
    }

    /** Escapes a Java string into a JSON string literal. */
    static String jsonEscape(String s) {
        StringBuilder sb = new StringBuilder(s.length() + 8);
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            switch (c) {
                case '"': sb.append("\\\""); break;
                case '\\': sb.append("\\\\"); break;
                case '\n': sb.append("\\n"); break;
                case '\r': sb.append("\\r"); break;
                case '\t': sb.append("\\t"); break;
                default:
                    if (c < 0x20) {
                        sb.append(String.format("\\u%04x", (int) c));
                    } else {
                        sb.append(c);
                    }
            }
        }
        return sb.toString();
    }

    /** Builds the messages array expected by the Rust side. */
    static String messagesJson(java.util.List<Msg> history) {
        StringBuilder sb = new StringBuilder("[");
        for (int i = 0; i < history.size(); i++) {
            Msg m = history.get(i);
            if (i > 0) {
                sb.append(',');
            }
            sb.append("{\"role\":\"").append(m.role)
              .append("\",\"content\":\"").append(jsonEscape(m.content)).append("\"}");
        }
        return sb.append(']').toString();
    }

    /**
     * Agent configuration as JSON (matches Rust `AgentConfig`).
     *
     * @param forceJson when true the model output is constrained by a GBNF grammar,
     *                  so a malformed tool call becomes impossible.
     */
    static String agentConfigJson(int maxSteps, boolean forceJson, String style,
                                  int maxTokens, float temperature, int threads,
                                  boolean react, int maxFailures) {
        return "{"
                + "\"mode\":\"" + (react ? "re_act" : "auto") + "\","
                + "\"max_steps\":" + maxSteps + ","
                + "\"max_consecutive_failures\":" + maxFailures + ","
                + "\"force_json\":" + forceJson + ","
                + "\"max_observation_chars\":4000,"
                + "\"style\":\"" + (style == null || style.isEmpty() ? "auto" : style) + "\","
                + "\"gen\":" + paramsJson(maxTokens, temperature, 40, 0.95f, 1.08f, threads, true)
                + "}";
    }

    /** One chat message (role is "system" | "user" | "assistant"). */
    public static final class Msg {
        public final String role;
        public final String content;

        public Msg(String role, String content) {
            this.role = role;
            this.content = content;
        }
    }

    /**
     * Generation parameters as JSON. Defaults are the ones the Rust core uses,
     * passed explicitly so the settings dialog can override them.
     */
    static String paramsJson(int maxTokens, float temperature, int topK, float topP,
                             float repeatPenalty, int threads, boolean speculative) {
        return "{"
                + "\"max_tokens\":" + maxTokens + ","
                + "\"temperature\":" + temperature + ","
                + "\"top_k\":" + topK + ","
                + "\"top_p\":" + topP + ","
                + "\"min_p\":0.05,"
                + "\"repeat_penalty\":" + repeatPenalty + ","
                + "\"seed\":24211,"
                + "\"stop\":[],"
                + "\"n_threads\":" + threads + ","
                + "\"n_threads_batch\":" + threads + ","
                + "\"use_speculative\":" + speculative + ","
                + "\"draft_tokens\":4"
                + "}";
    }
}
