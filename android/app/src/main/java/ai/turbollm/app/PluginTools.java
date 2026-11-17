package ai.turbollm.app;

import android.content.Context;
import android.util.Log;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.io.BufferedReader;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.net.HttpURLConnection;
import java.net.URL;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Iterator;
import java.util.List;
import java.util.zip.ZipEntry;
import java.util.zip.ZipInputStream;

/**
 * Installs plugins straight from GitHub, Claude Code style:
 *
 * <pre>
 *   /plugin install owner/repo
 *   /plugin install https://github.com/owner/repo/tree/main/plugins/weather
 * </pre>
 *
 * A plugin is a folder with {@code turbollm.plugin.json} (see
 * {@code crates/turbollm-core/src/plugins.rs} for the schema). The app fetches the
 * manifest from {@code raw.githubusercontent.com}; if the plugin needs sibling
 * files (skills) it falls back to downloading the repository archive zip.
 *
 * <p>If a repo has no {@code turbollm.plugin.json} but does have
 * {@code .claude-plugin/plugin.json}, that manifest is accepted too (fields are
 * a superset), so existing Claude Code plugin repos install unchanged.
 */
public final class PluginTools {

    private static final String TAG = "TurboLLM.plugins";

    private PluginTools() {}

    /** Where installed plugins live: files/plugins/<name>/ */
    public static File pluginsDir(Context ctx) {
        File dir = new File(ctx.getFilesDir(), "plugins");
        //noinspection ResultOfMethodCallIgnored
        dir.mkdirs();
        return dir;
    }

    public static final class Result {
        public boolean ok;
        public String message = "";
        public String pluginJson = "";
        public String name = "";
        public List<String> tools = new ArrayList<>();
        public String prompt = "";
    }

    /** One MCP server declared by a plugin or typed in by the user. */
    public static final class McpSpec {
        public String name = "";
        public String url = "";
        public String command = "";
        public String[] args = new String[0];
        public java.util.Map<String, String> env = new java.util.HashMap<>();
        public java.util.Map<String, String> headers = new java.util.HashMap<>();

        public boolean isHttp() {
            return url != null && !url.isEmpty();
        }

        static McpSpec fromJson(JSONObject o) {
            McpSpec s = new McpSpec();
            s.name = o.optString("name", "mcp");
            s.url = o.optString("url", "");
            s.command = o.optString("command", "");
            JSONArray args = o.optJSONArray("args");
            if (args != null) {
                s.args = new String[args.length()];
                for (int i = 0; i < args.length(); i++) {
                    s.args[i] = args.optString(i);
                }
            }
            JSONObject env = o.optJSONObject("env");
            if (env != null) {
                for (Iterator<String> it = env.keys(); it.hasNext(); ) {
                    String k = it.next();
                    s.env.put(k, env.optString(k));
                }
            }
            JSONObject headers = o.optJSONObject("headers");
            if (headers != null) {
                for (Iterator<String> it = headers.keys(); it.hasNext(); ) {
                    String k = it.next();
                    s.headers.put(k, headers.optString(k));
                }
            }
            return s;
        }
    }

    /**
     * Install a plugin from a GitHub reference. Blocking: call from a worker
     * thread. Returns a Result with the validated manifest JSON.
     */
    public static Result install(Context ctx, String source) {
        Result r = new Result();
        String refs = NativeBridge.nativePluginSources(source);
        if (refs == null) {
            r.message = "не удалось разобрать ссылку";
            return r;
        }
        JSONObject src;
        try {
            src = new JSONObject(refs);
        } catch (JSONException e) {
            r.message = "ошибка: " + e.getMessage();
            return r;
        }
        if (!src.optBoolean("ok", false)) {
            r.message = src.optString("error", "некорректная ссылка на GitHub");
            return r;
        }

        // 1. Try the known manifest locations directly (cheap, one request).
        JSONArray urls = src.optJSONArray("manifest_urls");
        String manifestText = null;
        String usedUrl = null;
        if (urls != null) {
            for (int i = 0; i < urls.length(); i++) {
                String u = urls.optString(i);
                String body = httpText(u);
                if (body != null && body.trim().startsWith("{")) {
                    manifestText = body;
                    usedUrl = u;
                    break;
                }
            }
        }

        // 2. Fall back to the repository archive (needed for skills/ sibling files).
        File pluginDir = null;
        if (manifestText == null) {
            ArchiveResult ar = downloadArchive(ctx, src.optString("archive_url"),
                    src.optString("subdir", ""));
            if (ar.error != null) {
                r.message = "Манифест не найден (" + (usedUrl == null ? "нет turbollm.plugin.json"
                        : usedUrl) + ") и архив не скачался: " + ar.error;
                return r;
            }
            pluginDir = ar.dir;
            manifestText = ar.manifest;
            if (manifestText == null) {
                r.message = "В репозитории нет turbollm.plugin.json (или .claude-plugin/plugin.json)";
                return r;
            }
        }

        // 3. Validate in Rust (same code path as the CI tests).
        String check = NativeBridge.nativePluginCheck(manifestText);
        JSONObject checked;
        try {
            checked = new JSONObject(check == null ? "{}" : check);
        } catch (JSONException e) {
            r.message = "не удалось проверить манифест";
            return r;
        }
        if (!checked.optBoolean("ok", false)) {
            StringBuilder sb = new StringBuilder("Манифест отклонён:\n");
            JSONArray errs = checked.optJSONArray("errors");
            if (errs != null) {
                for (int i = 0; i < errs.length(); i++) {
                    sb.append("• ").append(errs.optString(i)).append('\n');
                }
            }
            r.message = sb.toString();
            return r;
        }

        JSONObject manifest = checked.optJSONObject("manifest");
        r.name = manifest == null ? "plugin" : manifest.optString("name", "plugin");

        // 4. Persist: manifest + skills text.
        File dest = new File(pluginsDir(ctx), r.name);
        try {
            //noinspection ResultOfMethodCallIgnored
            dest.mkdirs();
            writeFile(new File(dest, "turbollm.plugin.json"), manifestText);
            String skillText = "";
            JSONArray skills = manifest == null ? null : manifest.optJSONArray("skills");
            if (skills != null) {
                StringBuilder all = new StringBuilder();
                for (int i = 0; i < skills.length(); i++) {
                    File f = new File(dest, skills.optString(i) + "/SKILL.md");
                    if (f.isFile()) {
                        all.append(readFile(f)).append("\n\n");
                    }
                }
                skillText = all.toString();
            }
            JSONObject store = new JSONObject();
            store.put("manifest", manifest == null ? new JSONObject() : manifest);
            store.put("source", source);
            store.put("path", dest.getAbsolutePath());
            store.put("enabled", true);
            store.put("skill_text", skillText);
            writeFile(new File(dest, "installed.json"), store.toString());

            String promptJson = NativeBridge.nativePluginPrompt(store.toString());
            JSONObject prompt = new JSONObject(promptJson == null ? "{}" : promptJson);
            r.prompt = prompt.optString("prompt", "");
            JSONArray tools = prompt.optJSONArray("tools");
            if (tools != null) {
                for (int i = 0; i < tools.length(); i++) {
                    JSONObject t = tools.optJSONObject(i);
                    if (t != null) {
                        r.tools.add(t.optString("name"));
                    }
                }
            }
            r.pluginJson = store.toString();
            r.ok = true;
            r.message = "Плагин «" + r.name + "» установлен"
                    + (r.tools.isEmpty() ? "" : ", инструментов: " + r.tools.size());
            if (pluginDir != null && !pluginDir.equals(dest)) {
                copyDir(pluginDir, dest);
            }
        } catch (Exception e) {
            Log.e(TAG, "install failed", e);
            r.message = "Не удалось сохранить плагин: " + e.getMessage();
        }
        return r;
    }

    /** All installed plugins, newest first. */
    public static List<JSONObject> installed(Context ctx) {
        List<JSONObject> out = new ArrayList<>();
        File[] dirs = pluginsDir(ctx).listFiles();
        if (dirs == null) {
            return out;
        }
        for (File d : dirs) {
            File f = new File(d, "installed.json");
            if (!f.isFile()) {
                continue;
            }
            try {
                out.add(new JSONObject(readFile(f)));
            } catch (JSONException e) {
                Log.w(TAG, "bad installed.json in " + d);
            }
        }
        out.sort((a, b) -> a.optJSONObject("manifest") == null ? 1 : -1);
        return out;
    }

    public static void uninstall(Context ctx, String name) {
        deleteDir(new File(pluginsDir(ctx), name));
    }

    public static void setEnabled(Context ctx, String name, boolean enabled) {
        File f = new File(new File(pluginsDir(ctx), name), "installed.json");
        if (!f.isFile()) {
            return;
        }
        try {
            JSONObject o = new JSONObject(readFile(f));
            o.put("enabled", enabled);
            writeFile(f, o.toString());
        } catch (JSONException | IOException e) {
            Log.w(TAG, "toggle failed", e);
        }
    }

    /** MCP servers declared by a plugin manifest. */
    public static List<McpSpec> mcpServers(JSONObject installedPlugin) {
        List<McpSpec> out = new ArrayList<>();
        JSONObject m = installedPlugin.optJSONObject("manifest");
        JSONArray servers = m == null ? null : m.optJSONArray("mcp_servers");
        if (servers == null) {
            return out;
        }
        for (int i = 0; i < servers.length(); i++) {
            JSONObject o = servers.optJSONObject(i);
            if (o != null) {
                out.add(McpSpec.fromJson(o));
            }
        }
        return out;
    }

    /** HTTP tools declared by a plugin manifest, keyed by tool name. */
    public static void registerHttpTools(JSONObject installedPlugin, ToolExecutor executor) {
        JSONObject m = installedPlugin.optJSONObject("manifest");
        JSONArray tools = m == null ? null : m.optJSONArray("tools");
        if (tools == null) {
            return;
        }
        for (int i = 0; i < tools.length(); i++) {
            JSONObject t = tools.optJSONObject(i);
            if (t == null) {
                continue;
            }
            JSONObject http = t.optJSONObject("http");
            if (http != null) {
                executor.registerHttpTool(t.optString("name"), http);
            }
        }
    }

    // ------------------------------------------------------------------ helpers

    private static final class ArchiveResult {
        File dir;
        String manifest;
        String error;
    }

    /**
     * Download `github.com/owner/repo/archive/<ref>.zip`, unzip to a temp dir and
     * locate the manifest (preferring `subdir`).
     */
    private static ArchiveResult downloadArchive(Context ctx, String archiveUrl, String subdir) {
        ArchiveResult res = new ArchiveResult();
        if (archiveUrl == null || !archiveUrl.startsWith("https://")) {
            res.error = "нет ссылки на архив";
            return res;
        }
        File tmp = new File(ctx.getCacheDir(), "plugin-dl");
        deleteDir(tmp);
        //noinspection ResultOfMethodCallIgnored
        tmp.mkdirs();
        try {
            HttpURLConnection conn = (HttpURLConnection) new URL(archiveUrl).openConnection();
            conn.setConnectTimeout(20000);
            conn.setReadTimeout(60000);
            conn.setRequestProperty("User-Agent", "TurboLLM/0.2");
            conn.setInstanceFollowRedirects(true);
            int code = conn.getResponseCode();
            if (code >= 400) {
                res.error = "HTTP " + code;
                return res;
            }
            long total = 0;
            try (ZipInputStream zis = new ZipInputStream(conn.getInputStream())) {
                ZipEntry e;
                while ((e = zis.getNextEntry()) != null) {
                    File out = new File(tmp, e.getName());
                    String canonical = out.getCanonicalPath();
                    if (!canonical.startsWith(tmp.getCanonicalPath() + File.separator)) {
                        continue; // zip-slip protection
                    }
                    if (e.isDirectory()) {
                        //noinspection ResultOfMethodCallIgnored
                        out.mkdirs();
                        continue;
                    }
                    total += e.getSize();
                    if (total > 32L * 1024 * 1024) {
                        res.error = "архив больше 32 МБ";
                        return res;
                    }
                    //noinspection ResultOfMethodCallIgnored
                    out.getParentFile().mkdirs();
                    try (FileOutputStream fos = new FileOutputStream(out)) {
                        byte[] buf = new byte[8192];
                        int n;
                        while ((n = zis.read(buf)) > 0) {
                            fos.write(buf, 0, n);
                        }
                    }
                }
            }
            File root = firstDir(tmp);
            if (root == null) {
                res.error = "пустой архив";
                return res;
            }
            File base = subdir == null || subdir.isEmpty() ? root : new File(root, subdir);
            File[] candidates = {
                    new File(base, "turbollm.plugin.json"),
                    new File(base, ".claude-plugin/plugin.json"),
                    new File(root, "turbollm.plugin.json"),
                    new File(root, ".claude-plugin/plugin.json"),
            };
            for (File c : candidates) {
                if (c.isFile()) {
                    res.manifest = readFile(c);
                    res.dir = c.getParentFile();
                    if (c.getParentFile().getName().equals(".claude-plugin")) {
                        res.dir = c.getParentFile().getParentFile();
                    }
                    return res;
                }
            }
            res.dir = base;
        } catch (Exception e) {
            res.error = e.getMessage();
        }
        return res;
    }

    private static File firstDir(File dir) {
        File[] files = dir.listFiles();
        if (files == null) {
            return null;
        }
        for (File f : files) {
            if (f.isDirectory()) {
                return f;
            }
        }
        return dir;
    }

    static String httpText(String url) {
        try {
            HttpURLConnection conn = (HttpURLConnection) new URL(url).openConnection();
            conn.setConnectTimeout(15000);
            conn.setReadTimeout(20000);
            conn.setInstanceFollowRedirects(true);
            conn.setRequestProperty("User-Agent", "TurboLLM/0.2");
            if (conn.getResponseCode() >= 400) {
                return null;
            }
            StringBuilder sb = new StringBuilder();
            try (BufferedReader br = new BufferedReader(
                    new InputStreamReader(conn.getInputStream(), StandardCharsets.UTF_8))) {
                String line;
                while ((line = br.readLine()) != null && sb.length() < 512 * 1024) {
                    sb.append(line).append('\n');
                }
            }
            return sb.toString();
        } catch (Exception e) {
            return null;
        }
    }

    static String readFile(File f) {
        try (FileInputStream in = new FileInputStream(f)) {
            byte[] buf = new byte[(int) f.length()];
            int n = in.read(buf);
            return new String(buf, 0, Math.max(n, 0), StandardCharsets.UTF_8);
        } catch (IOException e) {
            return "";
        }
    }

    static void writeFile(File f, String text) throws IOException {
        //noinspection ResultOfMethodCallIgnored
        f.getParentFile().mkdirs();
        try (FileOutputStream out = new FileOutputStream(f)) {
            out.write(text.getBytes(StandardCharsets.UTF_8));
        }
    }

    private static void copyDir(File from, File to) {
        if (from == null || !from.isDirectory()) {
            return;
        }
        File[] files = from.listFiles();
        if (files == null) {
            return;
        }
        for (File f : files) {
            File target = new File(to, f.getName());
            if (f.isDirectory()) {
                copyDir(f, target);
            } else {
                try (InputStream in = new FileInputStream(f);
                     FileOutputStream out = new FileOutputStream(target)) {
                    byte[] buf = new byte[8192];
                    int n;
                    while ((n = in.read(buf)) > 0) {
                        out.write(buf, 0, n);
                    }
                } catch (IOException ignored) {
                    // best effort: a missing skill file must not fail the install
                }
            }
        }
    }

    private static void deleteDir(File f) {
        if (f == null || !f.exists()) {
            return;
        }
        File[] files = f.listFiles();
        if (files != null) {
            for (File c : files) {
                deleteDir(c);
            }
        }
        //noinspection ResultOfMethodCallIgnored
        f.delete();
    }
}
