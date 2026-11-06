package dev.vegameta.rawzero.camera

import android.graphics.ImageFormat
import android.graphics.SurfaceTexture
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CameraMetadata
import android.hardware.camera2.CaptureRequest
import android.hardware.camera2.CaptureResult
import android.hardware.camera2.TotalCaptureResult
import android.hardware.camera2.params.OutputConfiguration
import android.media.Image
import android.media.ImageReader
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.util.Size
import android.view.Surface
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.withTimeout
import java.util.concurrent.Executor
import kotlin.math.abs

/**
 * One still: RAW frame (null for ISP-only lenses), the ISP JPEG of the same request (null
 * when the lens has no JPEG stream next to RAW), and the capture result of *this lens*
 * (the physical camera's result for lenses reached through a logical camera).
 */
class Shot(
    val lens: Lens,
    val raw: Image?,
    val ispJpeg: ByteArray?,
    val result: CaptureResult,
    val total: TotalCaptureResult,
    val jpegOrientation: Int,
    private val onClose: () -> Unit = {},
) : AutoCloseable {
    override fun close() {
        raw?.close()
        onClose()
    }
}

/**
 * One open camera: ISP preview on a PRIVATE surface, plus RAW_SENSOR (max) and JPEG
 * (<= RAW size) readers used only on shutter. PRIV preview + JPEG + RAW is a guaranteed
 * stream combination for RAW-capable devices. For a physical member of a logical camera
 * every stream is routed to that lens. No RAW work happens while previewing.
 */
class CaptureSession private constructor(
    val lens: Lens,
    private val thread: HandlerThread,
    private val handler: Handler,
    private val device: CameraDevice,
    private val session: CameraCaptureSession,
    private val preview: Surface,
    private val rawReader: ImageReader?,
    private val jpegReader: ImageReader?,
) : AutoCloseable {

    @Volatile var controls = Controls()
        private set

    /** Shots still being processed; readers outlive the session until they are released. */
    private var inFlight = 0
    private var closed = false
    private val lock = Any()

    /** Latest preview results metered with auto AE / auto AWB: references for manual modes. */
    @Volatile private var lastAutoAe: TotalCaptureResult? = null
    @Volatile private var lastAutoAwb: TotalCaptureResult? = null

    private var fpsWindowStart = 0L
    private var fpsFrames = 0

    private val previewCallback = object : CameraCaptureSession.CaptureCallback() {
        override fun onCaptureCompleted(s: CameraCaptureSession, r: CaptureRequest, res: TotalCaptureResult) {
            if (r.get(CaptureRequest.CONTROL_AE_MODE) != CameraMetadata.CONTROL_AE_MODE_OFF) lastAutoAe = res
            if (r.get(CaptureRequest.CONTROL_AWB_MODE) != CameraMetadata.CONTROL_AWB_MODE_OFF) lastAutoAwb = res
            // Preview health in logcat: frame rate and exposure every ~3 s.
            val now = System.nanoTime()
            fpsFrames++
            if (fpsWindowStart == 0L) fpsWindowStart = now
            if (now - fpsWindowStart >= 3_000_000_000L) {
                Log.i("RawZero", "preview %.1f fps, exp %s ms".format(
                    fpsFrames * 1e9 / (now - fpsWindowStart), res.get(CaptureResult.SENSOR_EXPOSURE_TIME)?.div(1_000_000)))
                fpsWindowStart = now
                fpsFrames = 0
            }
        }
    }

    companion object {
        private fun map(lens: Lens) = lens.chars[CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP]!!

        fun rawSize(lens: Lens): Size = map(lens).getOutputSizes(ImageFormat.RAW_SENSOR).maxBy { it.width.toLong() * it.height }

        /** Sensor aspect from the active array (works for ISP-only lenses too). */
        private fun aspect(lens: Lens): Double {
            if (lens.raw) rawSize(lens).let { return it.width.toDouble() / it.height }
            val a = lens.chars[CameraCharacteristics.SENSOR_INFO_ACTIVE_ARRAY_SIZE]
            return if (a != null && a.height() > 0) a.width().toDouble() / a.height() else 4.0 / 3
        }

        /** Largest preview size with the sensor aspect, capped for ISP load. */
        fun previewSize(lens: Lens): Size {
            val a = aspect(lens)
            return map(lens).getOutputSizes(SurfaceTexture::class.java)
                .filter { it.width <= 1920 && it.height <= 1440 && abs(it.width.toDouble() / it.height - a) < 0.02 }
                .maxByOrNull { it.width.toLong() * it.height } ?: Size(1440, 1080)
        }

        /**
         * Still readers: RAW (max) when the lens has RAW, JPEG when [withJpeg]. The JPEG is
         * no larger than RAW, same aspect: a full-resolution quad-Bayer JPEG next to the
         * binned RAW is not a guaranteed combination (session hangs on Xiaomi).
         */
        fun readers(lens: Lens, withJpeg: Boolean): List<ImageReader> {
            val out = mutableListOf<ImageReader>()
            if (lens.raw) rawSize(lens).let { out += ImageReader.newInstance(it.width, it.height, ImageFormat.RAW_SENSOR, 2) }
            if (withJpeg) {
                val sizes = map(lens).getOutputSizes(ImageFormat.JPEG)
                val a = aspect(lens)
                val limit = if (lens.raw) rawSize(lens) else Size(Int.MAX_VALUE, Int.MAX_VALUE)
                val jpeg = sizes.filter { it.width <= limit.width && it.height <= limit.height && abs(it.width.toDouble() / it.height - a) < 0.02 }
                    .maxByOrNull { it.width.toLong() * it.height } ?: sizes.maxBy { it.width.toLong() * it.height }
                out += ImageReader.newInstance(jpeg.width, jpeg.height, ImageFormat.JPEG, 2)
            }
            return out
        }

        suspend fun open(cm: CameraManager, lens: Lens, preview: Surface, controls: Controls): CaptureSession {
            val thread = HandlerThread("camera-${lens.key}").apply { start() }
            val handler = Handler(thread.looper)
            val exec = Executor { handler.post(it) }
            val readers = readers(lens, lens.ispJpeg || !lens.raw)
            val rawReader = readers.firstOrNull { it.imageFormat == ImageFormat.RAW_SENSOR }
            val jpegReader = readers.firstOrNull { it.imageFormat == ImageFormat.JPEG }
            var device: CameraDevice? = null
            try {
                device = withTimeout(5_000) { cm.openAwait(lens.cameraId, handler) }
                val outs = (listOf(preview) + readers.map { it.surface }).map { s ->
                    OutputConfiguration(s).apply { lens.physicalId?.let { setPhysicalCameraId(it) } }
                }
                val session = withTimeout(5_000) { device.createSessionAwait(outs, exec) }
                return CaptureSession(lens, thread, handler, device, session, preview, rawReader, jpegReader).also { it.setControls(controls) }
            } catch (e: Throwable) {
                device?.close()
                readers.forEach(ImageReader::close)
                thread.quitSafely()
                throw e
            }
        }
    }

    /** Re-issues the repeating preview request with new settings. */
    fun setControls(c: Controls) {
        controls = c
        val req = device.createCaptureRequest(CameraDevice.TEMPLATE_PREVIEW).apply {
            addTarget(preview)
            applyControls(lens, c, lastAutoAe, lastAutoAwb, preview = true)
        }.build()
        session.setRepeatingRequest(req, previewCallback, handler)
    }

    /** One still request writing RAW and/or the ISP JPEG. */
    suspend fun shoot(jpegOrientation: Int): Shot {
        rawReader?.let(::drain)
        jpegReader?.let(::drain)
        val rawImage = CompletableDeferred<Image>()
        val jpegBytes = CompletableDeferred<ByteArray>()
        rawReader?.setOnImageAvailableListener({ r -> r.acquireNextImage()?.let { if (!rawImage.complete(it)) it.close() } }, handler)
        jpegReader?.setOnImageAvailableListener({ r ->
            r.acquireNextImage()?.use { img ->
                val buf = img.planes[0].buffer
                jpegBytes.complete(ByteArray(buf.remaining()).also { buf.get(it) })
            }
        }, handler)
        val still = device.createCaptureRequest(CameraDevice.TEMPLATE_STILL_CAPTURE).apply {
            rawReader?.let { addTarget(it.surface) }
            jpegReader?.let { addTarget(it.surface) }
            applyControls(lens, controls, lastAutoAe, lastAutoAwb, preview = false)
            set(CaptureRequest.STATISTICS_LENS_SHADING_MAP_MODE, CameraMetadata.STATISTICS_LENS_SHADING_MAP_MODE_ON)
            set(CaptureRequest.JPEG_ORIENTATION, jpegOrientation)
            set(CaptureRequest.JPEG_QUALITY, 95.toByte())
        }.build()
        // Long exposures: the frame needs the exposure itself plus readout and processing.
        val budgetMs = 10_000L + 2 * (controls.exposureNs ?: 0L) / 1_000_000
        return withTimeout(budgetMs) {
            val t0 = System.nanoTime()
            val total = session.captureAwait(still, handler)
            // A physical stream's metadata (black level, WB, shading map) is in its own result.
            val result: CaptureResult = lens.physicalId?.let { total.physicalCameraTotalResults[it] } ?: total
            Log.i("RawZero", "capture took ${(System.nanoTime() - t0) / 1_000_000} ms, " +
                "result exp ${result.get(CaptureResult.SENSOR_EXPOSURE_TIME)} iso ${result.get(CaptureResult.SENSOR_SENSITIVITY)}")
            synchronized(lock) { inFlight++ }
            val shot = Shot(lens, rawReader?.let { rawImage.await() }, jpegReader?.let { jpegBytes.await() }, result, total, jpegOrientation) {
                synchronized(lock) {
                    inFlight--
                    if (closed && inFlight == 0) closeReaders()
                }
            }
            // Re-arm the preview so no still-only state (long exposure, vendor priority) lingers.
            runCatching { setControls(controls) }
            shot
        }
    }

    private fun drain(r: ImageReader) {
        while (true) r.acquireNextImage()?.close() ?: break
    }

    /**
     * Releases the camera at once (onPause must free it for other apps), but closing an
     * ImageReader invalidates its Images, so readers wait for shots still being processed.
     */
    override fun close() {
        runCatching { session.stopRepeating() }
        runCatching { session.close() }
        runCatching { device.close() }
        synchronized(lock) {
            closed = true
            if (inFlight == 0) closeReaders()
        }
    }

    private fun closeReaders() {
        rawReader?.close()
        jpegReader?.close()
        thread.quitSafely()
    }
}
