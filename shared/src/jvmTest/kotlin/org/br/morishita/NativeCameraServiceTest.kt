package org.br.morishita

import kotlin.test.Test
import kotlin.test.assertEquals

class NativeCameraServiceTest {
    // Goes through the same path as the app: JNA extracts the library from the jar
    // resources and the UniFFI bindings call into it
    @Test
    fun loadsTheNativeLibraryFromTheClasspath() {
        val service = NativeCameraService()
        val isWindows = System.getProperty("os.name").lowercase().contains("win")

        assertEquals(isWindows, service.previewSupported)
        assertEquals(0u, service.streamingClients())
    }
}
