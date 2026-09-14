package org.br.morishita

import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.awt.SwingPanel
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.IntSize
import com.sun.jna.Native
import com.sun.jna.Pointer
import java.awt.Canvas

@Composable
fun CameraView(onCanvasReady: (hwndPtr: ULong, width: UInt, height: UInt) -> Unit,
               onSizeChanged: (width: UInt, height: UInt) -> Unit,
               modifier: Modifier = Modifier
) {
    val density = LocalDensity.current
    var currentSizePixels by remember { mutableStateOf(IntSize.Zero) }

    // Get Compose's layout size and calculate physical resolution in the GPU
    val measuredModifier = modifier.onGloballyPositioned { coordinates ->
        val logicalSize = coordinates.size

        // Applying DPI scale factor (ex: 125%, 150%) to real pixels
        val physicalWidth = (logicalSize.width * density.density).toInt()
        val physicalHeight = (logicalSize.height * density.density).toInt()

        if (physicalWidth > 0 && physicalHeight > 0 &&
            (physicalWidth != currentSizePixels.width || physicalHeight != currentSizePixels.height)
        ) {
            currentSizePixels = IntSize(physicalWidth, physicalHeight)
            onSizeChanged(physicalWidth.toUInt(), physicalHeight.toUInt())
        }
    }

    SwingPanel(
        modifier = Modifier.fillMaxSize(),
        factory = {
            object : Canvas() {
                init {
                    ignoreRepaint = true
                }

                override fun addNotify() {
                    super.addNotify()
                    val jnaPointer = Native.getComponentPointer(this)
                    if (jnaPointer != null) {
                        val hwndAddress = Pointer.nativeValue(jnaPointer)
                        if (hwndAddress != 0L) {
                            val initialWidth = if (currentSizePixels.width > 0) currentSizePixels.width.toUInt() else 860u
                            val initialHeight = if (currentSizePixels.height > 0) currentSizePixels.height.toUInt() else 480u

                            // Emite o HWND e as dimensões para a inicialização no Rust
                            onCanvasReady(hwndAddress.toULong(), initialWidth, initialHeight)
                        }
                    }
                }
            }
        },
        update = { canvas ->
            canvas.invalidate()
        }
    )
}