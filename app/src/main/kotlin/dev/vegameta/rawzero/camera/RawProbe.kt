package dev.vegameta.rawzero.camera

import android.graphics.ImageFormat
import android.graphics.SurfaceTexture
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CameraMetadata
import android.hardware.camera2.CaptureRequest
import android.hardware.camera2.params.OutputConfiguration
import android.media.ImageReader
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.util.Size
import android.view.Surface
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.delay
import kotlinx.coroutines.withTimeout
import kotlinx.coroutines.withTimeoutOrNull
import java.util.concurrent.Executor

/**
 * Finds a way to get RAW from a lens whose HAL advertises RAW_SENSOR but delivers nothing
 * in our default setup (seen on a Nothing Phone 3a Pro: the still request completes, the
 * RAW buffer never arrives). Variants, in order of how much they change:
 *  - RAW size: the largest advertised, the pre-correction active array, the pixel array;
 *  - session with or without the ISP JPEG stream next to RAW;
 *  - still request alone, with the preview surface as an extra target, or as TEMPLATE_PREVIEW.
 * Every attempt is logged into [log] so a remote user's report shows what the HAL does.
 */
object RawProbe {

    fun sizes(lens: Lens): List<Size> {
        val ch = lens.chars
        val all = ch[CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP]
            ?.getOutputSizes(ImageFormat.RAW_SENSOR)?.toList().orEmpty()
        val pre = ch[CameraCharacteristics.SENSOR_INFO_PRE_CORRECTION_ACTIVE_ARRAY_SIZE]
        val px = ch[CameraCharacteristics.SENSOR_INFO_PIXEL_ARRAY_SIZE]
        val byArea = all.sortedByDescending { it.width.toLong() * it.height }
        val preferred = listOfNotNull(
            byArea.firstOrNull(),
            pre?.let { p -> all.find { it.width == p.width() && it.height == p.height() } },
            px?.let { p -> all.find { it.width == p.width && it.height == p.height } },
        ) + byArea
        return preferred.distinct().take(3)
    }

    /** Returns the lens with a RAW setup that delivered a frame, or null. Closes everything it opens. */
    suspend fun find(cm: CameraManager, lens: Lens, log: StringBuilder, progress: (String) -> Unit): Lens? {
        val thread = HandlerThread("probe-${lens.key}").apply { start() }
        val handler = Handler(thread.looper)
        val exec = Executor { handler.post(it) }
        val texture = SurfaceTexture(0)
        val pv = CaptureSession.previewSize(lens)
        texture.setDefaultBufferSize(pv.width, pv.height)
        val preview = Surface(texture)
        var device: CameraDevice? = null
        val ch = lens.chars
        log.appendLine("RAW sizes ${ch[CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP]?.getOutputSizes(ImageFormat.RAW_SENSOR)?.joinToString()}; " +
            "pre-correction ${ch[CameraCharacteristics.SENSOR_INFO_PRE_CORRECTION_ACTIVE_ARRAY_SIZE]}; pixel array ${ch[CameraCharacteristics.SENSOR_INFO_PIXEL_ARRAY_SIZE]}; " +
            "active ${ch[CameraCharacteristics.SENSOR_INFO_ACTIVE_ARRAY_SIZE]}; preview $pv")
        log.appendLine("RAW10 ${ch[CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP]?.getOutputSizes(ImageFormat.RAW10)?.joinToString()}; " +
            "RAW12 ${ch[CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP]?.getOutputSizes(ImageFormat.RAW12)?.joinToString()}; " +
            "physicalId ${lens.physicalId}; physical ids ${runCatching { ch.physicalCameraIds.joinToString() }.getOrNull()}")
        try {
            device = withTimeout(5_000) { cm.openAwait(lens.cameraId, handler) }
            var n = 0
            val sizes = sizes(lens)
            val total = sizes.size * (if (lens.ispJpeg) 2 else 1) * 4
            for (size in sizes) for (withJpeg in listOf(true, false)) {
                if (withJpeg && !lens.ispJpeg) continue
                val cand = lens.copy(rawW = size.width, rawH = size.height, ispJpeg = withJpeg, stillWithPreview = false, previewTemplate = false)
                val readers = CaptureSession.readers(cand, withJpeg)
                val rawR = readers.first { it.imageFormat == ImageFormat.RAW_SENSOR }
                val jpegR = readers.firstOrNull { it.imageFormat == ImageFormat.JPEG }
                val what = "RAW ${size.width}x${size.height}${jpegR?.let { " + JPEG ${it.width}x${it.height}" } ?: ""}"
                try {
                    val outs = (listOf(preview) + readers.map { it.surface }).map { s ->
                        OutputConfiguration(s).apply { lens.physicalId?.let { setPhysicalCameraId(it) } }
                    }
                    val s = runCatching { withTimeoutOrNull(4_000) { device.createSessionAwait(outs, exec) } }
                        .onFailure { log.appendLine("$what: session ${it.message}") }.getOrNull()
                    if (s == null) { log.appendLine("$what: session not configured"); continue }
                    try {
                        s.setRepeatingRequest(device.createCaptureRequest(CameraDevice.TEMPLATE_PREVIEW).apply {
                            addTarget(preview)
                            set(CaptureRequest.CONTROL_MODE, CameraMetadata.CONTROL_MODE_AUTO)
                        }.build(), null, handler)
                        delay(1_000) // let 3A settle like a real shot
                        for ((withPreview, tpl) in listOf(false to false, true to false, false to true, true to true)) {
                            progress("подбираю режим RAW: попытка ${++n} из $total")
                            val r = attempt(device, s, rawR, jpegR, preview, withPreview, tpl, handler)
                            log.appendLine("$what, previewInStill=$withPreview, previewTemplate=$tpl: $r")
                            Log.i("RawZero", "probe ${lens.key} $what pv=$withPreview tpl=$tpl: $r")
                            if (r.startsWith("OK")) return cand.copy(stillWithPreview = withPreview, previewTemplate = tpl).also { it.label = lens.label }
                        }
                    } finally {
                        runCatching { s.close() }
                        delay(300)
                    }
                } catch (e: Throwable) {
                    log.appendLine("$what: ${e.javaClass.simpleName} ${e.message}")
                } finally {
                    readers.forEach(ImageReader::close)
                }
            }
            return null
        } catch (e: Throwable) {
            log.appendLine("probe: ${e.javaClass.simpleName} ${e.message}")
            return null
        } finally {
            runCatching { device?.close() }
            preview.release()
            texture.release()
            thread.quitSafely()
        }
    }

    private suspend fun attempt(
        device: CameraDevice, s: CameraCaptureSession, rawR: ImageReader, jpegR: ImageReader?,
        preview: Surface, withPreview: Boolean, previewTemplate: Boolean, handler: Handler,
    ): String {
        for (r in listOfNotNull(rawR, jpegR)) while (true) { (runCatching { r.acquireLatestImage() }.getOrNull() ?: break).close() }
        val raw = CompletableDeferred<Unit>()
        val jpeg = CompletableDeferred<Unit>()
        rawR.setOnImageAvailableListener({ r -> r.acquireNextImage()?.close(); raw.complete(Unit) }, handler)
        jpegR?.setOnImageAvailableListener({ r -> r.acquireNextImage()?.close(); jpeg.complete(Unit) }, handler)
        val req = device.createCaptureRequest(if (previewTemplate) CameraDevice.TEMPLATE_PREVIEW else CameraDevice.TEMPLATE_STILL_CAPTURE).apply {
            addTarget(rawR.surface)
            jpegR?.let { addTarget(it.surface) }
            if (withPreview) addTarget(preview)
            set(CaptureRequest.CONTROL_MODE, CameraMetadata.CONTROL_MODE_AUTO)
        }.build()
        val t0 = System.nanoTime()
        val res = runCatching { withTimeoutOrNull(4_000) { s.captureAwait(req, handler) } }
        val gotRaw = withTimeoutOrNull(3_000) { raw.await() } != null
        val gotJpeg = jpegR?.let { withTimeoutOrNull(if (gotRaw) 2_000 else 500) { jpeg.await() } != null }
        val ms = (System.nanoTime() - t0) / 1_000_000
        val result = res.fold({ if (it != null) "result" else "no result" }, { "failed: ${it.message}" })
        val ok = gotRaw && gotJpeg != false
        return "${if (ok) "OK" else "no"} $result raw=$gotRaw jpeg=$gotJpeg ${ms}ms"
    }
}
