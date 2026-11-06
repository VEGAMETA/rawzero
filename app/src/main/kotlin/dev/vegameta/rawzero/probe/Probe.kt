package dev.vegameta.rawzero.probe

import android.content.Context
import android.graphics.ImageFormat
import android.hardware.camera2.CameraAccessException
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CameraMetadata
import android.hardware.camera2.CaptureRequest
import android.hardware.camera2.CaptureResult
import android.hardware.camera2.DngCreator
import android.hardware.camera2.TotalCaptureResult
import android.hardware.camera2.params.OutputConfiguration
import android.hardware.camera2.params.SessionConfiguration
import android.media.Image
import android.media.ImageReader
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.util.Size
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout
import java.io.File
import java.io.FileOutputStream
import java.util.concurrent.Executor
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException

private const val TAG = "RawZeroProbe"
private const val MAX_PROBE_ID = 127

class Probe(private val ctx: Context, private val out: File, private val ui: (String) -> Unit) {
    private val cm = ctx.getSystemService(CameraManager::class.java)
    private val report = StringBuilder()

    private suspend fun log(s: String) {
        Log.i(TAG, s)
        report.append(s).append('\n')
        withContext(Dispatchers.Main) { ui(s) }
    }

    suspend fun run(capture: Boolean) {
        log("package ${ctx.packageName}")
        val public = cm.cameraIdList.toList()
        log("getCameraIdList: $public")

        // Characteristics for every ID the framework will answer for, listed or not.
        val found = linkedMapOf<String, CameraCharacteristics>()
        for (id in (public + (0..MAX_PROBE_ID).map { it.toString() }).distinct()) {
            runCatching { cm.getCameraCharacteristics(id) }
                .onSuccess { found[id] = it }
                .onFailure { if (id in public) log("  $id: characteristics failed: $it") }
        }
        // Physical members of logical cameras, even when not listed themselves.
        for (ch in found.values.toList()) {
            for (p in ch.physicalCameraIds) if (p !in found) {
                runCatching { cm.getCameraCharacteristics(p) }.onSuccess { found[p] = it }
                    .onFailure { log("  physical $p: characteristics failed: $it") }
            }
        }
        log("")
        for ((id, ch) in found) log(describe(id, ch, id in public))

        if (capture) {
            for ((id, ch) in found) {
                // Hidden logical cameras are vendor-internal; opening Xiaomi's logical ID 5
                // crashed the camera provider. Probe only listed logical cameras.
                if (isLogical(ch) && id !in public) {
                    log("$id: hidden logical camera, skipped")
                    continue
                }
                if (hasRaw(ch)) captureSafely(id, ch, null)
                for (p in ch.physicalCameraIds) {
                    val pch = found[p] ?: continue
                    if (hasRaw(pch)) captureSafely(id, ch, p)
                }
            }
        }
        File(out, "report.txt").writeText(report.toString())
        log("\ndone: ${out.absolutePath}")
    }

    private fun isLogical(ch: CameraCharacteristics) =
        ch[CameraCharacteristics.REQUEST_AVAILABLE_CAPABILITIES]
            ?.contains(CameraMetadata.REQUEST_AVAILABLE_CAPABILITIES_LOGICAL_MULTI_CAMERA) == true

    private fun hasRaw(ch: CameraCharacteristics): Boolean {
        val caps = ch[CameraCharacteristics.REQUEST_AVAILABLE_CAPABILITIES] ?: return false
        val map = ch[CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP] ?: return false
        return CameraMetadata.REQUEST_AVAILABLE_CAPABILITIES_RAW in caps &&
            map.outputFormats.contains(ImageFormat.RAW_SENSOR)
    }

    private fun describe(id: String, ch: CameraCharacteristics, listed: Boolean): String {
        val facing = when (ch[CameraCharacteristics.LENS_FACING]) {
            CameraMetadata.LENS_FACING_BACK -> "back"
            CameraMetadata.LENS_FACING_FRONT -> "front"
            CameraMetadata.LENS_FACING_EXTERNAL -> "ext"
            else -> "?"
        }
        val focal = ch[CameraCharacteristics.LENS_INFO_AVAILABLE_FOCAL_LENGTHS]?.joinToString("/") { "%.2f".format(it) }
        val phys = ch[CameraCharacteristics.SENSOR_INFO_PHYSICAL_SIZE]?.let { "%.2fx%.2f".format(it.width, it.height) }
        val px = ch[CameraCharacteristics.SENSOR_INFO_PIXEL_ARRAY_SIZE]
        val caps = ch[CameraCharacteristics.REQUEST_AVAILABLE_CAPABILITIES]?.toSet() ?: emptySet()
        val flags = buildList {
            if (hasRaw(ch)) add("RAW")
            if (CameraMetadata.REQUEST_AVAILABLE_CAPABILITIES_LOGICAL_MULTI_CAMERA in caps) add("LOGICAL${ch.physicalCameraIds}")
            if (CameraMetadata.REQUEST_AVAILABLE_CAPABILITIES_SYSTEM_CAMERA in caps) add("SYSTEM")
            if (CameraMetadata.REQUEST_AVAILABLE_CAPABILITIES_ULTRA_HIGH_RESOLUTION_SENSOR in caps) add("UHR")
        }
        val raw = ch[CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP]
            ?.takeIf { it.outputFormats.contains(ImageFormat.RAW_SENSOR) }
            ?.getOutputSizes(ImageFormat.RAW_SENSOR)?.joinToString(",")
        val shading = ch[CameraCharacteristics.SENSOR_INFO_LENS_SHADING_APPLIED]
        val zoom = ch[CameraCharacteristics.CONTROL_ZOOM_RATIO_RANGE]
        return "%-4s %-6s %-5s f=%s sensor=%s px=%s %s raw=%s shadingApplied=%s zoom=%s".format(
            id, if (listed) "listed" else "hidden", facing, focal, phys, px, flags, raw, shading, zoom,
        )
    }

    private suspend fun captureSafely(id: String, ch: CameraCharacteristics, physical: String?) {
        val name = if (physical == null) "cam$id" else "cam$id-phys$physical"
        try {
            withTimeout(15_000) { captureRaw(id, ch, physical, name) }
        } catch (e: Throwable) {
            log("$name: FAILED ${e.javaClass.simpleName}: ${e.message}")
        }
    }

    private suspend fun captureRaw(id: String, ch: CameraCharacteristics, physical: String?, name: String) {
        val thread = HandlerThread("probe-$name").apply { start() }
        val handler = Handler(thread.looper)
        val exec = Executor { handler.post(it) }
        val streamCh = physical?.let { cm.getCameraCharacteristics(it) } ?: ch
        val map = streamCh[CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP]!!
        val rawSize = map.getOutputSizes(ImageFormat.RAW_SENSOR).maxBy { it.width.toLong() * it.height }
        val yuvSize = map.getOutputSizes(ImageFormat.YUV_420_888)
            .filter { it.width <= 1280 }.maxByOrNull { it.width.toLong() * it.height } ?: Size(640, 480)
        val rawReader = ImageReader.newInstance(rawSize.width, rawSize.height, ImageFormat.RAW_SENSOR, 2)
        val yuvReader = ImageReader.newInstance(yuvSize.width, yuvSize.height, ImageFormat.YUV_420_888, 4)
        yuvReader.setOnImageAvailableListener({ r -> r.acquireLatestImage()?.close() }, handler)
        val rawImage = CompletableDeferred<Image>()
        rawReader.setOnImageAvailableListener({ r -> r.acquireNextImage()?.let { rawImage.complete(it) } }, handler)

        var device: CameraDevice? = null
        var session: CameraCaptureSession? = null
        try {
            device = openCamera(id, handler)
            val outputs = listOf(rawReader, yuvReader).map { r ->
                OutputConfiguration(r.surface).apply { physical?.let { setPhysicalCameraId(it) } }
            }
            session = createSession(device, outputs, exec)

            // Let AE/AF/AWB converge on a small YUV stream; the RAW stream stays idle.
            val preview = device.createCaptureRequest(CameraDevice.TEMPLATE_PREVIEW).apply {
                addTarget(yuvReader.surface)
                set(CaptureRequest.CONTROL_MODE, CameraMetadata.CONTROL_MODE_AUTO)
            }.build()
            session.setRepeatingRequest(preview, null, handler)
            delay(2000)

            val result = CompletableDeferred<TotalCaptureResult>()
            val still = device.createCaptureRequest(CameraDevice.TEMPLATE_STILL_CAPTURE).apply {
                addTarget(rawReader.surface)
                set(CaptureRequest.STATISTICS_LENS_SHADING_MAP_MODE, CameraMetadata.STATISTICS_LENS_SHADING_MAP_MODE_ON)
            }.build()
            session.capture(still, object : CameraCaptureSession.CaptureCallback() {
                override fun onCaptureCompleted(s: CameraCaptureSession, r: CaptureRequest, res: TotalCaptureResult) {
                    result.complete(res)
                }
                override fun onCaptureFailed(s: CameraCaptureSession, r: CaptureRequest, f: android.hardware.camera2.CaptureFailure) {
                    result.completeExceptionally(IllegalStateException("capture failed, reason ${f.reason}"))
                }
            }, handler)
            val total = result.await()
            val image = rawImage.await()
            val res: CaptureResult = physical?.let { total.physicalCameraTotalResults[it] } ?: total
            log("$name: ${image.width}x${image.height} " + resultSummary(res))
            image.use { writeDng(name, streamCh, res, total, it) }
        } finally {
            runCatching { session?.close() }
            runCatching { device?.close() }
            rawReader.close()
            yuvReader.close()
            thread.quitSafely()
        }
    }

    private suspend fun writeDng(name: String, ch: CameraCharacteristics, res: CaptureResult, total: TotalCaptureResult, image: Image) {
        val file = File(out, "$name.dng")
        val attempt = { r: CaptureResult ->
            DngCreator(ch, r).use { dng -> FileOutputStream(file).use { dng.writeImage(it, image) } }
        }
        try {
            attempt(res)
        } catch (e: Exception) {
            if (res === total) throw e
            log("$name: DngCreator with physical result failed ($e), retrying with logical result")
            attempt(total)
        }
        log("$name: wrote ${file.name} (${file.length() / 1024} KB)")
    }

    private fun resultSummary(r: CaptureResult): String {
        val nr = r[CaptureResult.NOISE_REDUCTION_MODE]
        val hp = r[CaptureResult.HOT_PIXEL_MODE]
        val iso = r[CaptureResult.SENSOR_SENSITIVITY]
        val exp = r[CaptureResult.SENSOR_EXPOSURE_TIME]?.let { "%.1fms".format(it / 1e6) }
        val black = r[CaptureResult.SENSOR_DYNAMIC_BLACK_LEVEL]?.joinToString("/") { "%.1f".format(it) }
        val white = r[CaptureResult.SENSOR_DYNAMIC_WHITE_LEVEL]
        val lsm = r[CaptureResult.STATISTICS_LENS_SHADING_CORRECTION_MAP]?.let { "${it.columnCount}x${it.rowCount}" }
        val gains = r[CaptureResult.COLOR_CORRECTION_GAINS]
        val zoom = r[CaptureResult.CONTROL_ZOOM_RATIO]
        return "iso=$iso exp=$exp nr=$nr hotPixel=$hp black=$black white=$white lsm=$lsm gains=$gains zoom=$zoom"
    }

    private suspend fun openCamera(id: String, handler: Handler): CameraDevice =
        suspendCancellableCoroutine { cont ->
            try {
                cm.openCamera(id, object : CameraDevice.StateCallback() {
                    override fun onOpened(d: CameraDevice) {
                        if (cont.isActive) cont.resume(d) else d.close()
                    }
                    override fun onDisconnected(d: CameraDevice) {
                        d.close()
                        if (cont.isActive) cont.resumeWithException(CameraAccessException(CameraAccessException.CAMERA_DISCONNECTED, "disconnected"))
                    }
                    override fun onError(d: CameraDevice, error: Int) {
                        d.close()
                        if (cont.isActive) cont.resumeWithException(IllegalStateException("open error $error"))
                    }
                }, handler)
            } catch (e: Exception) {
                if (cont.isActive) cont.resumeWithException(e)
            }
        }

    private suspend fun createSession(device: CameraDevice, outputs: List<OutputConfiguration>, exec: Executor): CameraCaptureSession =
        suspendCancellableCoroutine { cont ->
            val cfg = SessionConfiguration(SessionConfiguration.SESSION_REGULAR, outputs, exec,
                object : CameraCaptureSession.StateCallback() {
                    override fun onConfigured(s: CameraCaptureSession) {
                        if (cont.isActive) cont.resume(s) else s.close()
                    }
                    override fun onConfigureFailed(s: CameraCaptureSession) {
                        if (cont.isActive) cont.resumeWithException(IllegalStateException("session configuration failed"))
                    }
                })
            try {
                device.createCaptureSession(cfg)
            } catch (e: Exception) {
                if (cont.isActive) cont.resumeWithException(e)
            }
        }
}
