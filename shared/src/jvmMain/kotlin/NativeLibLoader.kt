import com.sun.jna.Native
import com.sun.jna.NativeLibrary
import java.io.File
import java.util.concurrent.atomic.AtomicBoolean

object NativeLibLoader {
    private val isLoaded = AtomicBoolean(false)

    fun ensureLoaded() {
        if (isLoaded.getAndSet(true)) return

        val crateName = "camera_engine" // Nome exato definido no Cargo.toml

        val os = System.getProperty("os.name").lowercase()
        val libFileName = when {
            os.contains("win") -> "$crateName.dll"
            os.contains("mac") -> "lib$crateName.dylib"
            else -> "lib$crateName.so"
        }

        try {
            // 1. Tenta usar a extração nativa do JNA dos recursos do JAR
            NativeLibrary.getInstance(crateName)
        } catch (e: Exception) {
            // 2. Fallback manual seguro para pasta temporária do sistema
            println("failed: ${e.message}")
            val stream = NativeLibLoader::class.java.getResourceAsStream("/$libFileName")
                ?: throw IllegalStateException("Could not found $libFileName in JVM resources.")

            val tempFile = File.createTempFile("native_", "_$libFileName").apply { deleteOnExit() }
            stream.use { input -> tempFile.outputStream().use { output -> input.copyTo(output) } }
            System.load(tempFile.absolutePath)
        }
    }
}