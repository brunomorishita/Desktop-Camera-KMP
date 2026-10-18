package org.br.morishita

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

class NativeCameraServiceTest {
    // Goes through the same path as the app: JNA extracts the library from the jar
    // resources and the UniFFI bindings call into it
    @Test
    fun loadsTheNativeLibraryFromTheClasspath() {
        val service = NativeCameraService()
        val isWindows = System.getProperty("os.name").lowercase().contains("win")

        // On Linux it depends on the machine (X11 display, VA-API GPU)
        if (isWindows) assertTrue(service.previewSupported)
        assertEquals(0u, service.streamingClients())
    }
}
