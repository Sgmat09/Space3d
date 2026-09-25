package android.content;

/**
 * Desktop stand-in for {@code android.content.Context}, just enough for open-android-models'
 * {@code OamJni} in a desktop JVM (tests/android_jvm.rs). Not shipped anywhere.
 */
public class Context {
    public Context getApplicationContext() {
        return this;
    }

    public ClassLoader getClassLoader() {
        return Context.class.getClassLoader();
    }
}
