package ai.turbollm.app;

import android.util.Log;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.net.HttpURLConnection;
import java.net.URL;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicLong;

/**
 * Executes tools the Rust core cannot: HTTP tools from plugin manifests, Android
 * APIs, and MCP servers (stdio child processes or HTTP+SSE).
 *
 * <p>The Rust side calls {@link #execute(String, String)} for every tool call the
 * model makes and expects a JSON string {@code {"ok":bool,"output":"..."}}.
 */
public final class ToolExecutor {

    private static final String TAG = "TurboLLM.tools";

    /** Supplies device facts to the `device_info` tool (set by MainActivity). */
    public interface DeviceInfo {
        String describe();
    }

    private final DeviceInfo deviceInfo;
    private final Map<String, McpClient> mcpClients = new ConcurrentHashMap<>();
    /** Plugin HTTP tools, keyed by tool name. */
    private final Map<String, JSONObject> httpTools = new ConcurrentHashMap<>();

    public ToolExecutor(DeviceInfo deviceInfo) {
        this.deviceInfo = deviceInfo;
    }

    // ------------------------------------------------------------- registration

    public void registerHttpTool(String name, JSONObject httpSpec) {
        httpTools.put(name, httpSpec);
    }

    public void registerMcpServer(String serverName, PluginTools.McpSpec spec) {
        McpClient client = new McpClient(serverName, spec);
        mcpClients.put(serverName, client);
    }

    public void clear() {
        for (McpClient c : mcpClients.values()) {
            c.close();
        }
        mcpClients.clear();
        httpTools.clear();
    }

    /** Connect to every registered MCP server and return their tools as JSON. */
    public String listMcpTools() {
        JSONArray all = new JSONArray();
        for (Map.Entry<String, McpClient> e : mcpClients.entrySet()) {
            try {
                JSONArray tools = e.getValue().listTools();
                for (int i = 0; i < tools.length(); i++) {
                    all.put(tools.get(i));
                }
            } catch (Exception ex) {
                Log.w(TAG, "MCP " + e.getKey() + " failed: " + ex);
            }
        }
        return all.toString();
    }

    // ---------------------------------------------------------------- execution

    /** Entry point called from Rust (JNI). Never throws. */
    public String execute(String name, String argsJson) {
        JSONObject args;
        try {
            args = new JSONObject(argsJson == null || argsJson.isEmpty() ? "{}" : argsJson);
        } catch (JSONException e) {
            args = new JSONObject();
        }
        try {
            String result;
            if ("device_info".equals(name)) {
                result = deviceInfo != null ? deviceInfo.describe() : "н/д";
            } else if (httpTools.containsKey(name)) {
                result = runHttp(httpTools.get(name), args);
            } else if (name.contains("__")) {
                String[] parts = name.split("__", 2);
                McpClient client = mcpClients.get(parts[0]);
                if (client == null) {
                    return error("MCP-сервер «" + parts[0] + "» не подключён");
                }
                result = client.callTool(parts[1], args);
            } else {
                return error("инструмент «" + name + "» неизвестен приложению");
            }
            return new JSONObject().put("ok", true).put("output", result).toString();
        } catch (Exception e) {
            Log.w(TAG, "tool " + name + " failed", e);
            return error(String.valueOf(e.getMessage()));
        }
    }

    private static String error(String msg) {
        try {
            return new JSONObject().put("ok", false).put("output", msg).toString();
        } catch (JSONException e) {
            return "{\"ok\":false,\"output\":\"unknown error\"}";
        }
    }

    private String runHttp(JSONObject spec, JSONObject args) throws Exception {
        String method = spec.optString("method", "GET");
        String url = substitute(spec.getString("url"), args);
        if (!url.startsWith("https://")) {
            throw new IllegalArgumentException("разрешён только https://");
        }
        int maxBytes = spec.optInt("max_bytes", 8000);
        HttpURLConnection conn = (HttpURLConnection) new URL(url).openConnection();
        conn.setRequestMethod(method);
        conn.setConnectTimeout(15000);
        conn.setReadTimeout(20000);
        conn.setRequestProperty("User-Agent", "TurboLLM/0.2 (+android)");
        JSONObject headers = spec.optJSONObject("headers");
        if (headers != null) {
            for (java.util.Iterator<String> it = headers.keys(); it.hasNext(); ) {
                String k = it.next();
                conn.setRequestProperty(k, substitute(headers.optString(k), args));
            }
        }
        if (!"GET".equalsIgnoreCase(method) && spec.has("body")) {
            conn.setDoOutput(true);
            byte[] body = substitute(spec.getString("body"), args)
                    .getBytes(StandardCharsets.UTF_8);
            conn.setRequestProperty("Content-Type", "application/json");
            try (OutputStream os = conn.getOutputStream()) {
                os.write(body);
            }
        }
        int code = conn.getResponseCode();
        java.io.InputStream is = code >= 400 ? conn.getErrorStream() : conn.getInputStream();
        StringBuilder sb = new StringBuilder();
        if (is != null) {
            try (BufferedReader br = new BufferedReader(
                    new InputStreamReader(is, StandardCharsets.UTF_8))) {
                char[] buf = new char[4096];
                int n;
                while ((n = br.read(buf)) > 0 && sb.length() < maxBytes) {
                    sb.append(buf, 0, n);
                }
            }
        }
        conn.disconnect();
        String body = sb.length() > maxBytes ? sb.substring(0, maxBytes) : sb.toString();
        if (code >= 400) {
            throw new IllegalStateException("HTTP " + code + ": " + body);
        }
        return body;
    }

    /** Replace `{param}` / `{param.path}` / `{param[0]}` with values from args. */
    static String substitute(String template, JSONObject args) {
        StringBuilder out = new StringBuilder();
        int i = 0;
        while (i < template.length()) {
            char c = template.charAt(i);
            int open = template.indexOf('{', i);
            if (c != '{' || open < 0) {
                out.append(c);
                i++;
                continue;
            }
            int close = template.indexOf('}', open);
            if (close < 0) {
                out.append(template.substring(i));
                break;
            }
            out.append(template, i, open);
            String key = template.substring(open + 1, close);
            out.append(lookup(args, key));
            i = close + 1;
        }
        return out.toString();
    }

    private static String lookup(JSONObject args, String path) {
        String[] parts = path.split("[.\\[\\]]");
        Object cur = args;
        for (String p : parts) {
            if (p.isEmpty()) {
                continue;
            }
            if (cur instanceof JSONObject) {
                cur = ((JSONObject) cur).opt(p);
            } else if (cur instanceof JSONArray) {
                try {
                    cur = ((JSONArray) cur).opt(Integer.parseInt(p));
                } catch (Exception e) {
                    return "";
                }
            } else {
                return "";
            }
        }
        return cur == null || cur == JSONObject.NULL ? "" : String.valueOf(cur);
    }

    // ------------------------------------------------------------------- MCP

    /** Minimal MCP client: stdio child process or HTTP JSON-RPC. */
    static final class McpClient {
        private final String name;
        private final PluginTools.McpSpec spec;
        private final AtomicLong ids = new AtomicLong(1);
        private Process process;
        private BufferedReader stdout;
        private OutputStream stdin;
        private boolean initialized;

        McpClient(String name, PluginTools.McpSpec spec) {
            this.name = name;
            this.spec = spec;
        }

        synchronized void ensureStarted() throws Exception {
            if (initialized) {
                return;
            }
            if (spec.isHttp()) {
                initialized = true; // stateless: nothing to start
                return;
            }
            if (spec.command == null || spec.command.isEmpty()) {
                throw new IllegalStateException("у MCP-сервера «" + name + "» нет command/url");
            }
            List<String> cmd = new ArrayList<>();
            cmd.add(spec.command);
            cmd.addAll(Arrays.asList(spec.args));
            ProcessBuilder pb = new ProcessBuilder(cmd);
            if (spec.env != null) {
                pb.environment().putAll(spec.env);
            }
            pb.redirectErrorStream(false);
            process = pb.start();
            stdout = new BufferedReader(new InputStreamReader(process.getInputStream(),
                    StandardCharsets.UTF_8));
            stdin = process.getOutputStream();
            initialized = true;
        }

        synchronized JSONArray listTools() throws Exception {
            ensureStarted();
            JSONObject resp = request("tools/list", new JSONObject());
            JSONObject result = resp.optJSONObject("result");
            String namespaced = NativeBridge.nativeMcpTools(name,
                    result == null ? "{}" : result.toString());
            return new JSONArray(namespaced == null || namespaced.isEmpty() ? "[]" : namespaced);
        }

        synchronized String callTool(String tool, JSONObject args) throws Exception {
            ensureStarted();
            JSONObject params = new JSONObject()
                    .put("name", tool)
                    .put("arguments", args);
            JSONObject resp = request("tools/call", params);
            JSONObject result = resp.optJSONObject("result");
            String out = NativeBridge.nativeMcpResult(
                    result == null ? "{}" : result.toString());
            JSONObject parsed = new JSONObject(out == null ? "{}" : out);
            String text = parsed.optString("output", "");
            if (!parsed.optBoolean("ok", true)) {
                throw new IllegalStateException(text);
            }
            return text;
        }

        /** Send one JSON-RPC request and wait for the matching response. */
        private JSONObject request(String method, JSONObject params) throws Exception {
            long id = ids.getAndIncrement();
            if (spec.isHttp()) {
                return httpRequest(id, method, params);
            }
            String line = NativeBridge.nativeMcpRequest(id, method, params.toString());
            stdin.write(line.getBytes(StandardCharsets.UTF_8));
            stdin.flush();
            long deadline = System.currentTimeMillis() + 30000;
            while (System.currentTimeMillis() < deadline) {
                String resp = stdout.readLine();
                if (resp == null) {
                    throw new IllegalStateException("MCP-сервер «" + name + "» закрыл поток");
                }
                JSONObject parsed = new JSONObject(
                        NativeBridge.nativeMcpParse(resp));
                if (parsed.optBoolean("notification", false)) {
                    continue; // server notification, ignore
                }
                if (!parsed.optBoolean("ok", true)) {
                    throw new IllegalStateException("MCP " + name + ": "
                            + parsed.optString("error", "ошибка"));
                }
                JSONObject wrapped = new JSONObject();
                wrapped.put("result", parsed.opt("result"));
                return wrapped;
            }
            throw new IllegalStateException("MCP-сервер «" + name + "» не ответил за 30 с");
        }

        private JSONObject httpRequest(long id, String method, JSONObject params)
                throws Exception {
            String body = NativeBridge.nativeMcpRequest(id, method, params.toString());
            HttpURLConnection conn = (HttpURLConnection) new URL(spec.url).openConnection();
            conn.setRequestMethod("POST");
            conn.setDoOutput(true);
            conn.setConnectTimeout(15000);
            conn.setReadTimeout(40000);
            conn.setRequestProperty("Content-Type", "application/json");
            conn.setRequestProperty("Accept", "application/json, text/event-stream");
            if (spec.headers != null) {
                for (Map.Entry<String, String> h : spec.headers.entrySet()) {
                    conn.setRequestProperty(h.getKey(), h.getValue());
                }
            }
            try (OutputStream os = conn.getOutputStream()) {
                os.write(body.getBytes(StandardCharsets.UTF_8));
            }
            int code = conn.getResponseCode();
            java.io.InputStream is = code >= 400 ? conn.getErrorStream() : conn.getInputStream();
            StringBuilder sb = new StringBuilder();
            if (is != null) {
                try (BufferedReader br = new BufferedReader(
                        new InputStreamReader(is, StandardCharsets.UTF_8))) {
                    String l;
                    while ((l = br.readLine()) != null) {
                        sb.append(l).append('\n');
                    }
                }
            }
            conn.disconnect();
            String text = sb.toString().trim();
            if (text.startsWith("event:") || text.startsWith("data:")) {
                // SSE: pick the first data payload that parses as JSON-RPC.
                for (String line : text.split("\n")) {
                    String t = line.trim();
                    if (t.startsWith("data:")) {
                        t = t.substring(5).trim();
                    }
                    if (t.startsWith("{")) {
                        text = t;
                        break;
                    }
                }
            }
            JSONObject parsed = new JSONObject(NativeBridge.nativeMcpParse(text));
            if (!parsed.optBoolean("ok", true)) {
                throw new IllegalStateException("MCP " + name + ": "
                        + parsed.optString("error", "ошибка HTTP " + code));
            }
            JSONObject wrapped = new JSONObject();
            wrapped.put("result", parsed.opt("result"));
            return wrapped;
        }

        void close() {
            if (process != null) {
                process.destroy();
                process = null;
            }
        }
    }

    /** Merges built-in + plugin + MCP tool specs into one JSON array. */
    static String mergeTools(List<String> jsonArrays) {
        Map<String, JSONObject> byName = new LinkedHashMap<>();
        for (String arr : jsonArrays) {
            if (arr == null || arr.isEmpty()) {
                continue;
            }
            try {
                JSONArray a = new JSONArray(arr);
                for (int i = 0; i < a.length(); i++) {
                    JSONObject t = a.optJSONObject(i);
                    if (t != null && t.has("name")) {
                        byName.put(t.optString("name"), t);
                    }
                }
            } catch (JSONException e) {
                Log.w(TAG, "bad tools json", e);
            }
        }
        return new JSONArray(byName.values()).toString();
    }
}
