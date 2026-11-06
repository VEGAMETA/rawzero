package dev.vegameta.rawzero.camera

import android.graphics.ImageFormat
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CameraMetadata
import android.hardware.camera2.CaptureRequest
import android.hardware.camera2.TotalCaptureResult
import android.hardware.camera2.params.OutputConfiguration
import android.media.Image
import android.media.ImageReader
import android.os.Handler
import android.os.HandlerThread
import android.util.Size
import android.view.Surface
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.withTimeout
import java.util.concurrent.Executor

/** RAW frame with the ISP JPEG of the same request and the full capture result. */
class Shot(val lens: Lens, val raw: Image, val ispJpeg: ByteArray, val result: TotalCaptureResult, val jpegOrientation: Int) :
    AutoCloseable {
    override fun close() = raw.close()
}

/**
 * One open camera: ISP preview on a PRIVATE surface, plus RAW_SENSOR (max) and JPEG (max)
 * readers used only on shutter. PRIV preview + JPEG max + RAW max is a guaranteed stream
 * combination for RAW-capable devices (CameraDevice#createCaptureSession tables).
 * 3A stays automatic; no RAW work happens while previewing.
 */
class CaptureSession private constructor(
    val lens: Lens,
    private val thread: HandlerThread,
    private val handler: Handler,
    private val device: CameraDevice,
    private val session: CameraCaptureSession,
    private val rawReader: ImageReader,
    private val jpegReader: ImageReader,
) : AutoCloseable {

    companion object {
        /** Largest preview size with the sensor aspect, capped for ISP load. */
        fun previewSize(lens: Lens): Size {
            val map = lens.chars[CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP]!!
            val raw = rawSize(lens)
            val aspect = raw.width.toDouble() / raw.height
            return map.getOutputSizes(android.graphics.SurfaceTexture::class.java)
                .filter { it.width <= 1920 && it.height <= 1440 && kotlin.math.abs(it.width.toDouble() / it.height - aspect) < 0.02 }
                .maxByOrNull { it.width.toLong() * it.height } ?: Size(1440, 1080)
        }

        fun rawSize(lens: Lens): Size =
            lens.chars[CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP]!!
                .getOutputSizes(ImageFormat.RAW_SENSOR).maxBy { it.width.toLong() * it.height }

        suspend fun open(cm: CameraManager, lens: Lens, preview: Surface): CaptureSession {
            val thread = HandlerThread("camera-${lens.id}").apply { start() }
            val handler = Handler(thread.looper)
            val exec = Executor { handler.post(it) }
            val map = lens.chars[CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP]!!
            val raw = rawSize(lens)
            // JPEG no larger than RAW, same aspect: a full-resolution quad-Bayer JPEG next to
            // the binned RAW is not a guaranteed combination (session hangs on Xiaomi).
            val jpeg = map.getOutputSizes(ImageFormat.JPEG)
                .filter { it.width <= raw.width && it.height <= raw.height &&
                    kotlin.math.abs(it.width.toDouble() / it.height - raw.width.toDouble() / raw.height) < 0.02 }
                .maxByOrNull { it.width.toLong() * it.height }
                ?: map.getOutputSizes(ImageFormat.JPEG).minBy { kotlin.math.abs(it.width - raw.width) }
            val rawReader = ImageReader.newInstance(raw.width, raw.height, ImageFormat.RAW_SENSOR, 2)
            val jpegReader = ImageReader.newInstance(jpeg.width, jpeg.height, ImageFormat.JPEG, 2)
            var device: CameraDevice? = null
            try {
                device = withTimeout(5_000) { cm.openAwait(lens.id, handler) }
                val session = withTimeout(5_000) { device.createSessionAwait(
                    listOf(preview, rawReader.surface, jpegReader.surface).map { OutputConfiguration(it) }, exec) }
                val req = device.createCaptureRequest(CameraDevice.TEMPLATE_PREVIEW).apply {
                    addTarget(preview)
                    setAuto(lens.chars)
                }.build()
                session.setRepeatingRequest(req, null, handler)
                return CaptureSession(lens, thread, handler, device, session, rawReader, jpegReader)
            } catch (e: Throwable) {
                device?.close()
                rawReader.close()
                jpegReader.close()
                thread.quitSafely()
                throw e
            }
        }

        private fun CaptureRequest.Builder.setAuto(ch: CameraCharacteristics) {
            set(CaptureRequest.CONTROL_MODE, CameraMetadata.CONTROL_MODE_AUTO)
            set(CaptureRequest.CONTROL_AE_MODE, CameraMetadata.CONTROL_AE_MODE_ON)
            set(CaptureRequest.CONTROL_AWB_MODE, CameraMetadata.CONTROL_AWB_MODE_AUTO)
            val af = ch[CameraCharacteristics.CONTROL_AF_AVAILABLE_MODES] ?: intArrayOf()
            if (CameraMetadata.CONTROL_AF_MODE_CONTINUOUS_PICTURE in af) {
                set(CaptureRequest.CONTROL_AF_MODE, CameraMetadata.CONTROL_AF_MODE_CONTINUOUS_PICTURE)
            }
        }
    }

    /** One still request writing both RAW and the ISP JPEG; the JPEG is the look reference. */
    suspend fun shoot(jpegOrientation: Int): Shot {
        drain(rawReader)
        drain(jpegReader)
        val rawImage = CompletableDeferred<Image>()
        val jpegBytes = CompletableDeferred<ByteArray>()
        rawReader.setOnImageAvailableListener({ r -> r.acquireNextImage()?.let { if (!rawImage.complete(it)) it.close() } }, handler)
        jpegReader.setOnImageAvailableListener({ r ->
            r.acquireNextImage()?.use { img ->
                val buf = img.planes[0].buffer
                jpegBytes.complete(ByteArray(buf.remaining()).also { buf.get(it) })
            }
        }, handler)
        val still = device.createCaptureRequest(CameraDevice.TEMPLATE_STILL_CAPTURE).apply {
            addTarget(rawReader.surface)
            addTarget(jpegReader.surface)
            setAuto(lens.chars)
            set(CaptureRequest.STATISTICS_LENS_SHADING_MAP_MODE, CameraMetadata.STATISTICS_LENS_SHADING_MAP_MODE_ON)
            set(CaptureRequest.JPEG_ORIENTATION, jpegOrientation)
            set(CaptureRequest.JPEG_QUALITY, 95.toByte())
        }.build()
        return withTimeout(10_000) {
            val result = session.captureAwait(still, handler)
            Shot(lens, rawImage.await(), jpegBytes.await(), result, jpegOrientation)
        }
    }

    private fun drain(r: ImageReader) {
        while (true) r.acquireNextImage()?.close() ?: break
    }

    override fun close() {
        runCatching { session.stopRepeating() }
        runCatching { session.close() }
        runCatching { device.close() }
        rawReader.close()
        jpegReader.close()
        thread.quitSafely()
    }
}
