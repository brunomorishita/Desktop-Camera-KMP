package org.br.morishita

import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.awt.SwingPanel
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.unit.IntSize
import com.sun.jna.Native
import com.sun.jna.Pointer
import java.awt.Canvas
import java.util.concurrent.atomic.AtomicLong

@Composable
fun CameraView(onCanvasReady: (hwndPtr: ULong, width: UInt, height: UInt) -> Unit,
               onCanvasDisposed: (hwndPtr: ULong) -> Unit,
               onSizeChanged: (hwndPtr: ULong, width: UInt, height: UInt) -> Unit,
               modifier: Modifier = Modifier
) {
    var currentSizePixels by remember { mutableStateOf(IntSize.Zero) }
    // Set on the AWT thread when the native window exists, 0 otherwise
    val canvasHwnd = remember { AtomicLong(0L) }

    // Canvas is created once, so it must always call the latest callbacks
    val currentOnCanvasReady by rememberUpdatedState(onCanvasReady)
    val currentOnCanvasDisposed by rememberUpdatedState(onCanvasDisposed)

    // Compose's layout size is already in physical pixels (DPI scale applied)
    val measuredModifier = modifier.onGloballyPositioned { coordinates ->
        val physicalWidth = coordinates.size.width
        val physicalHeight = coordinates.size.height

        if (physicalWidth > 0 && physicalHeight > 0 &&
            (physicalWidth != currentSizePixels.width || physicalHeight != currentSizePixels.height)
        ) {
            currentSizePixels = IntSize(physicalWidth, physicalHeight)
            val hwnd = canvasHwnd.get()
            if (hwnd != 0L) {
                onSizeChanged(hwnd.toULong(), physicalWidth.toUInt(), physicalHeight.toUInt())
            }
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
                            canvasHwnd.set(hwndAddress)
                            val initialWidth = if (currentSizePixels.width > 0) currentSizePixels.width.toUInt() else 860u
                            val initialHeight = if (currentSizePixels.height > 0) currentSizePixels.height.toUInt() else 480u

                            // Emite o HWND e as dimensões para a inicialização no Rust
                            currentOnCanvasReady(hwndAddress.toULong(), initialWidth, initialHeight)
                        }
                    }
                }

                override fun removeNotify() {
                    // Native rendering must stop while the HWND still exists
                    val hwndAddress = canvasHwnd.getAndSet(0L)
                    if (hwndAddress != 0L) {
                        currentOnCanvasDisposed(hwndAddress.toULong())
                    }
                    super.removeNotify()
                }
            }
        },
        update = { canvas ->
            canvas.invalidate()
        }
    )
}
