import com.sun.jna.NativeLibrary
import com.sun.jna.Platform
import java.util.concurrent.atomic.AtomicBoolean

object NativeLibLoader {
    private const val CRATE_NAME = "camera_engine" // Nome exato definido no Cargo.toml

    private val isLoaded = AtomicBoolean(false)

    // JNA extracts the library from the jar (<platform>/<file>, placed there by Gradle) and
    // the UniFFI bindings then find it by name, so loading it here only fails early and clearly
    fun ensureLoaded() {
        if (isLoaded.getAndSet(true)) return

        try {
            NativeLibrary.getInstance(CRATE_NAME)
        } catch (e: UnsatisfiedLinkError) {
            isLoaded.set(false)
            throw IllegalStateException(
                "Native library $CRATE_NAME not found for ${Platform.RESOURCE_PREFIX}: ${e.message}",
                e
            )
        }
    }
}
