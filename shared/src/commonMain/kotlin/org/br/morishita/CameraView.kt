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
import androidx.compose.ui.unit.IntSize
import com.sun.jna.Native
import com.sun.jna.Pointer
import java.awt.Canvas

@Composable
fun CameraView(onCanvasReady: (hwndPtr: ULong, width: UInt, height: UInt) -> Unit,
               onSizeChanged: (width: UInt, height: UInt) -> Unit,
               modifier: Modifier = Modifier
) {
    var currentSizePixels by remember { mutableStateOf(IntSize.Zero) }

    // Compose's layout size is already in physical pixels (DPI scale applied)
    val measuredModifier = modifier.onGloballyPositioned { coordinates ->
        val physicalWidth = coordinates.size.width
        val physicalHeight = coordinates.size.height

        if (physicalWidth > 0 && physicalHeight > 0 &&
            (physicalWidth != currentSizePixels.width || physicalHeight != currentSizePixels.height)
        ) {
            currentSizePixels = IntSize(physicalWidth, physicalHeight)
            onSizeChanged(physicalWidth.toUInt(), physicalHeight.toUInt())
        }
    }

    SwingPanel(
        modifier = measuredModifier.fillMaxSize(),
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