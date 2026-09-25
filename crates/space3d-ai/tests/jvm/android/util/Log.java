package android.util;

/** Desktop stand-in for {@code android.util.Log} (tests/android_jvm.rs): prints to stderr. */
public final class Log {
    private Log() {}

    private static int print(String level, String tag, String msg, Throwable tr) {
        System.err.println(level + "/" + tag + ": " + msg + (tr == null ? "" : " (" + tr + ")"));
        return 0;
    }

    public static int d(String tag, String msg) { return print("D", tag, msg, null); }
    public static int i(String tag, String msg) { return print("I", tag, msg, null); }
    public static int w(String tag, String msg) { return print("W", tag, msg, null); }
    public static int w(String tag, String msg, Throwable tr) { return print("W", tag, msg, tr); }
    public static int w(String tag, Throwable tr) { return print("W", tag, "", tr); }
    public static int e(String tag, String msg) { return print("E", tag, msg, null); }
    public static int e(String tag, String msg, Throwable tr) { return print("E", tag, msg, tr); }
}
