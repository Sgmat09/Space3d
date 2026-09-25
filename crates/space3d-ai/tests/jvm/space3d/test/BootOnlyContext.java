package space3d.test;

/** A Context whose class loader sees only the JDK: as if the open-android-models AAR were missing. */
public class BootOnlyContext extends android.content.Context {
    @Override
    public ClassLoader getClassLoader() {
        return new ClassLoader(null) {};
    }
}
