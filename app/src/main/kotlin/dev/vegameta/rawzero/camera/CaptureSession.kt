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
import kotlinx.coroutines.withTimeoutOrNull
import java.util.concurrent.Executor
import kotlin.math.abs

/** Per-frame exposure cap for night bursts (hand-held). */
const val NIGHT_MAX_EXPOSURE_NS = 125_000_000L
/** Frames per night burst. */
const val NIGHT_FRAMES = 8

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
    /** Zoom of the request (the RAW must be cropped to match the ISP outputs). */
    val zoom: Float = 1f,
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
            if (afWatch > 0) {
                afWatch--
                val st = res.get(CaptureResult.CONTROL_AF_STATE)
                if (st != lastAfState) {
                    lastAfState = st
                    Log.i("RawZero", "AF state $st (2 passive focused, 4 focused locked, 5 not focused locked) region ${res.get(CaptureResult.CONTROL_AF_REGIONS)?.firstOrNull()} " +
                        "lens ${res.get(CaptureResult.LENS_FOCUS_DISTANCE)} ae ${res.get(CaptureResult.CONTROL_AE_STATE)}")
                }
            }
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

        fun rawSize(lens: Lens): Size =
            if (lens.rawW > 0) Size(lens.rawW, lens.rawH)
            else map(lens).getOutputSizes(ImageFormat.RAW_SENSOR).maxBy { it.width.toLong() * it.height }

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
            if (lens.raw) rawSize(lens).let { out += ImageReader.newInstance(it.width, it.height, ImageFormat.RAW_SENSOR, 3) }
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

        suspend fun open(cm: CameraManager, lens: Lens, preview: Surface, controls: Controls, onLost: (String) -> Unit = {}): CaptureSession {
            val thread = HandlerThread("camera-${lens.key}").apply { start() }
            val handler = Handler(thread.looper)
            val exec = Executor { handler.post(it) }
            val readers = readers(lens, lens.ispJpeg || !lens.raw)
            val rawReader = readers.firstOrNull { it.imageFormat == ImageFormat.RAW_SENSOR }
            val jpegReader = readers.firstOrNull { it.imageFormat == ImageFormat.JPEG }
            var device: CameraDevice? = null
            try {
                device = withTimeout(5_000) { cm.openAwait(lens.cameraId, handler, onLost) }
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

    /**
     * Starts single-shot AF on the current meter point: cancel any previous scan, then
     * AF_TRIGGER_START; the repeating request (AF_MODE_AUTO with the region) keeps the result.
     */
    fun triggerFocus() {
        if (meteringRect(lens.chars, controls) == null) return
        val base = {
            device.createCaptureRequest(CameraDevice.TEMPLATE_PREVIEW).apply {
                addTarget(preview)
                applyControls(lens, controls, lastAutoAe, lastAutoAwb, preview = true)
            }
        }
        session.capture(base().apply { set(CaptureRequest.CONTROL_AF_TRIGGER, CameraMetadata.CONTROL_AF_TRIGGER_CANCEL) }.build(), null, handler)
        session.capture(base().apply { set(CaptureRequest.CONTROL_AF_TRIGGER, CameraMetadata.CONTROL_AF_TRIGGER_START) }.build(), previewCallback, handler)
        afWatch = 30
    }

    /** Frames left to log AF state after a tap (debugging aid). */
    @Volatile private var afWatch = 0
    private var lastAfState: Int? = null

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
    /**
     * Flash "on capture": torch on in the preview for a moment so AE/AWB settle under it,
     * then the still (also with torch), then the normal preview again (re-armed in shoot()).
     */
    private suspend fun preLight() {
        if (controls.flash != Flash.CAPTURE || !hasFlash(lens.chars)) return
        val req = device.createCaptureRequest(CameraDevice.TEMPLATE_PREVIEW).apply {
            addTarget(preview)
            applyControls(lens, controls, lastAutoAe, lastAutoAwb, preview = false)
        }.build()
        session.setRepeatingRequest(req, previewCallback, handler)
        kotlinx.coroutines.delay(if (controls.exposureManual) 250 else 700)
    }

    suspend fun shoot(jpegOrientation: Int): Shot {
        preLight()
        rawReader?.let(::drain)
        jpegReader?.let(::drain)
        val rawImage = CompletableDeferred<Image>()
        val jpegBytes = CompletableDeferred<ByteArray>()
        rawReader?.setOnImageAvailableListener({ r -> r.acquireNextImage()?.let { if (!rawImage.complete(it)) it.close() } }, handler)
        jpegReader?.setOnImageAvailableListener({ r ->
            r.acquireNextImage()?.use { img ->
                val buf = img.planes[0].buffer
                val bytes = ByteArray(buf.remaining()).also { buf.get(it) }
                if (!jpegBytes.isCompleted) jpegBytes.complete(bytes)
            }
        }, handler)
        val lsmOn = lens.chars[CameraCharacteristics.STATISTICS_INFO_AVAILABLE_LENS_SHADING_MAP_MODES]
            ?.contains(CameraMetadata.STATISTICS_LENS_SHADING_MAP_MODE_ON) == true
        fun still(withRaw: Boolean) = device.createCaptureRequest(
            if (withRaw && lens.previewTemplate) CameraDevice.TEMPLATE_PREVIEW else CameraDevice.TEMPLATE_STILL_CAPTURE).apply {
            if (withRaw) rawReader?.let { addTarget(it.surface) }
            if (withRaw && lens.stillWithPreview) addTarget(preview)
            jpegReader?.let { addTarget(it.surface) }
            applyControls(lens, controls, lastAutoAe, lastAutoAwb, preview = false)
            if (lsmOn && withRaw) set(CaptureRequest.STATISTICS_LENS_SHADING_MAP_MODE, CameraMetadata.STATISTICS_LENS_SHADING_MAP_MODE_ON)
            set(CaptureRequest.JPEG_ORIENTATION, jpegOrientation)
            set(CaptureRequest.JPEG_QUALITY, 95.toByte())
        }.build()
        // Long exposures: the frame needs the exposure itself plus readout and processing.
        val budgetMs = 8_000L + 2 * (controls.exposureNs ?: 0L) / 1_000_000
        val t0 = System.nanoTime()
        trace = "still RAW=${rawReader != null} JPEG=${jpegReader != null}"
        var total = withTimeoutOrNull(budgetMs) { session.captureAwait(still(rawReader != null), handler) }
        trace += total?.let { " result ${(System.nanoTime() - t0) / 1_000_000}ms" } ?: " NO RESULT"
        // Each output gets its own wait: some HALs advertise RAW + JPEG and then deliver only one.
        var raw = rawReader?.let { withTimeoutOrNull(4_000) { rawImage.await() } }
        var jpeg = jpegReader?.let { withTimeoutOrNull(if (raw != null) 4_000 else 1_000) { jpegBytes.await() } }
        trace += " raw=${raw != null} jpeg=${jpeg != null}"
        if (rawReader != null && raw == null) rawFailed = true
        if (raw == null && jpeg == null && jpegReader != null) {
            // Retry as the phone's own JPEG only (no RAW stream in the request).
            Log.w("RawZero", "still delivered nothing ($trace), retrying JPEG only")
            total = withTimeoutOrNull(budgetMs) { session.captureAwait(still(false), handler) }
            jpeg = withTimeoutOrNull(4_000) { jpegBytes.await() }
            trace += " | retry jpeg-only result=${total != null} jpeg=${jpeg != null}"
        }
        if (total == null || (raw == null && jpeg == null)) {
            runCatching { setControls(controls) }
            throw IllegalStateException("камера не отдала кадр ($trace)")
        }
        // A physical stream's metadata (black level, WB, shading map) is in its own result.
        val result: CaptureResult = lens.physicalId?.let { total.physicalCameraTotalResults[it] } ?: total
        Log.i("RawZero", "capture took ${(System.nanoTime() - t0) / 1_000_000} ms ($trace), " +
            "result exp ${result.get(CaptureResult.SENSOR_EXPOSURE_TIME)} iso ${result.get(CaptureResult.SENSOR_SENSITIVITY)} flash ${result.get(CaptureResult.FLASH_STATE)}")
        synchronized(lock) { inFlight++ }
        val zoom = controls.zoom.coerceIn(1f, zoomRange(lens.chars).first)
        val shot = Shot(lens, raw, jpeg, result, total, jpegOrientation, zoom) {
            synchronized(lock) {
                inFlight--
                if (closed && inFlight == 0) closeReaders()
            }
        }
        // Re-arm the preview so no still-only state (long exposure, vendor priority) lingers.
        runCatching { setControls(controls) }
        return shot
    }

    /** What the last still did, for error reports. */
    @Volatile var trace = ""
        private set

    /** The HAL advertised RAW but did not deliver it: the lens should become ISP-only. */
    @Volatile var rawFailed = false
        private set

    /**
     * Night burst: [frames] RAW-only stills with identical settings - the metered exposure,
     * capped at [NIGHT_MAX_EXPOSURE_NS] per frame against hand shake with ISO making up the
     * rest (the merge then averages the noise down); AWB locked and focus held so frames
     * agree. [onFrame] gets each image in order (with the result for frame 0) and must not
     * keep it: the reader is small and the HAL waits for free buffers.
     */
    suspend fun shootNight(frames: Int, jpegOrientation: Int, onFrame: suspend (Int, Image, CaptureResult?) -> Unit) =
        shootBurst(frames, null, null, onFrame)

    /**
     * Long exposure by summing [frames] RAW frames of [frameExposure] at [iso] each (for
     * exposures past the sensor's real one-frame limit).
     */
    suspend fun shootStack(frames: Int, frameExposure: Long, iso: Int, onFrame: suspend (Int, Image, CaptureResult?) -> Unit) =
        shootBurst(frames, frameExposure, iso, onFrame)

    /** Frames, per-frame exposure and ISO for a stacked long exposure of [total] ns. */
    fun stackPlan(total: Long): Triple<Int, Long, Int> {
        val ch = lens.chars
        val frameExp = minOf(realMaxExposure(ch), total)
        val frames = ((total + frameExp - 1) / frameExp).toInt().coerceIn(1, STACK_MAX_FRAMES)
        val ae = lastAutoAe
        val meteredExp = ae?.get(CaptureResult.SENSOR_EXPOSURE_TIME) ?: 33_000_000L
        val meteredIso = ae?.get(CaptureResult.SENSOR_SENSITIVITY) ?: 800
        val isoRange = ch[CameraCharacteristics.SENSOR_INFO_SENSITIVITY_RANGE]
        // Same brightness rule as one long frame: total time x ISO = metered time x ISO.
        val iso = (controls.iso ?: (meteredIso.toDouble() * meteredExp / total).toInt()).let { isoRange?.clamp(it) ?: it }
        return Triple(frames, frameExp, iso)
    }

    private suspend fun shootBurst(frames: Int, fixedExp: Long?, fixedIso: Int?, onFrame: suspend (Int, Image, CaptureResult?) -> Unit) {
        val reader = rawReader ?: error("burst needs RAW")
        preLight()
        drain(reader)
        val ae = lastAutoAe
        val ch = lens.chars
        val meteredExp = controls.exposureNs ?: ae?.get(CaptureResult.SENSOR_EXPOSURE_TIME) ?: 50_000_000L
        val meteredIso = controls.iso ?: ae?.get(CaptureResult.SENSOR_SENSITIVITY) ?: 800
        val expRange = ch[CameraCharacteristics.SENSOR_INFO_EXPOSURE_TIME_RANGE]
        val isoRange = ch[CameraCharacteristics.SENSOR_INFO_SENSITIVITY_RANGE]
        val exp = fixedExp ?: minOf(meteredExp, NIGHT_MAX_EXPOSURE_NS).let { expRange?.clamp(it) ?: it }
        val iso = fixedIso ?: (meteredIso.toDouble() * meteredExp / exp).toInt().let { isoRange?.clamp(it) ?: it }
        val focus = ae?.get(CaptureResult.LENS_FOCUS_DISTANCE)
        Log.i("RawZero", "burst: $frames x ${exp / 1_000_000} ms ISO $iso (metered ${meteredExp / 1_000_000} ms ISO $meteredIso)")
        val requests = (0 until frames).map {
            device.createCaptureRequest(if (lens.previewTemplate) CameraDevice.TEMPLATE_PREVIEW else CameraDevice.TEMPLATE_STILL_CAPTURE).apply {
                addTarget(reader.surface)
                if (lens.stillWithPreview) addTarget(preview)
                applyControls(lens, controls, lastAutoAe, lastAutoAwb, preview = false)
                set(CaptureRequest.CONTROL_AE_MODE, CameraMetadata.CONTROL_AE_MODE_OFF)
                set(CaptureRequest.SENSOR_EXPOSURE_TIME, exp)
                set(CaptureRequest.SENSOR_SENSITIVITY, iso)
                set(CaptureRequest.CONTROL_AWB_LOCK, true)
                if (focus != null && (ch[CameraCharacteristics.LENS_INFO_MINIMUM_FOCUS_DISTANCE] ?: 0f) > 0f) {
                    set(CaptureRequest.CONTROL_AF_MODE, CameraMetadata.CONTROL_AF_MODE_OFF)
                    set(CaptureRequest.LENS_FOCUS_DISTANCE, focus)
                }
                if (ch[CameraCharacteristics.STATISTICS_INFO_AVAILABLE_LENS_SHADING_MAP_MODES]?.contains(CameraMetadata.STATISTICS_LENS_SHADING_MAP_MODE_ON) == true)
                    set(CaptureRequest.STATISTICS_LENS_SHADING_MAP_MODE, CameraMetadata.STATISTICS_LENS_SHADING_MAP_MODE_ON)
            }.build()
        }
        // Back-pressure: the listener only signals; frames are acquired when the merge is ready
        // for them, so at most maxImages buffers are held and the HAL waits for the rest.
        val ready = kotlinx.coroutines.channels.Channel<Unit>(kotlinx.coroutines.channels.Channel.UNLIMITED)
        val first = CompletableDeferred<TotalCaptureResult>()
        reader.setOnImageAvailableListener({ ready.trySend(Unit) }, handler)
        val budgetMs = 15_000L + frames * (exp / 1_000_000) * 3
        try {
            withTimeout(budgetMs) {
                session.captureBurst(requests, object : CameraCaptureSession.CaptureCallback() {
                    override fun onCaptureCompleted(s: CameraCaptureSession, r: CaptureRequest, res: TotalCaptureResult) {
                        if (!first.isCompleted) first.complete(res)
                    }
                    override fun onCaptureFailed(s: CameraCaptureSession, r: CaptureRequest, f: android.hardware.camera2.CaptureFailure) {
                        if (!first.isCompleted) first.completeExceptionally(IllegalStateException("burst capture failed, reason ${f.reason}"))
                    }
                }, handler)
                for (k in 0 until frames) {
                    ready.receive()
                    val img = reader.acquireNextImage() ?: error("burst frame $k missing")
                    try {
                        val res: CaptureResult? = if (k == 0) {
                            val total = first.await()
                            lens.physicalId?.let { total.physicalCameraTotalResults[it] } ?: total
                        } else null
                        onFrame(k, img, res)
                    } finally {
                        img.close()
                    }
                }
            }
        } finally {
            reader.setOnImageAvailableListener(null, null)
            drain(reader)
            runCatching { setControls(controls) }
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
