package dev.vegameta.rawzero.camera

import android.content.Context
import android.graphics.ImageFormat
import android.graphics.SurfaceTexture
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CameraMetadata
import android.hardware.camera2.params.OutputConfiguration
import android.media.ImageReader
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.view.Surface
import kotlinx.coroutines.withTimeout
import java.util.concurrent.Executor
import kotlin.math.hypot
import kotlin.math.roundToInt

/**
 * One physical lens. [cameraId] is what gets opened; [physicalId] is set when the lens is
 * reachable only as a physical member of a logical camera (streams routed with
 * OutputConfiguration.setPhysicalCameraId). [chars] are the lens' own characteristics.
 */
data class Lens(
    val cameraId: String,
    val physicalId: String?,
    val front: Boolean,
    /** 35 mm equivalent focal length (diagonal crop factor). */
    val eqMm: Int,
    val listed: Boolean,
    /** RAW_SENSOR available: our pipeline. False: ISP JPEG only (fallback). */
    val raw: Boolean,
    /** The ISP JPEG stream fits in the session next to RAW (look reference). */
    val ispJpeg: Boolean,
    val chars: CameraCharacteristics,
    /** RAW stream size found by [RawProbe]; 0 = largest advertised. */
    val rawW: Int = 0,
    val rawH: Int = 0,
    /** The HAL fills RAW only when the still request also targets the preview surface. */
    val stillWithPreview: Boolean = false,
    /** The HAL fills RAW only for TEMPLATE_PREVIEW requests. */
    val previewTemplate: Boolean = false,
) {
    /** Sensor id for file names and logs. */
    val id: String get() = physicalId ?: cameraId
    val key: String get() = physicalId?.let { "$cameraId/$it" } ?: cameraId
    var label: String = if (front) "front" else "${eqMm}mm"
}

private const val TAG = "RawZero"
private const val MAX_ID = 255
private const val PREFS = "camera_catalog"
/** Bumped when [RawProbe] learns new variants: lenses demoted by an older probe are retried. */
private const val PROBE_VERSION = 1

/**
 * Finds every usable lens on any phone:
 *  1. candidates: every ID 0..[MAX_ID] that answers getCameraCharacteristics (vendors such
 *     as Xiaomi hide IDs from getCameraIdList), plus physical members of listed logical
 *     cameras (how Pixel/OnePlus and others expose their lenses);
 *  2. verification on first launch: each candidate is opened and a session is configured
 *     exactly as for shooting (preview + RAW + JPEG, falling back to preview + RAW);
 *  3. the result is cached per build fingerprint, so later launches are instant and an
 *     OS update triggers a rescan.
 * Hidden logical cameras are never opened (Xiaomi's logical ID 5 crashed the provider).
 * Lenses without RAW are kept as ISP-only so the phone stays usable.
 */
object CameraCatalog {

    fun hasRaw(ch: CameraCharacteristics): Boolean {
        val caps = ch[CameraCharacteristics.REQUEST_AVAILABLE_CAPABILITIES] ?: return false
        val map = ch[CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP] ?: return false
        return CameraMetadata.REQUEST_AVAILABLE_CAPABILITIES_RAW in caps && map.outputFormats.contains(ImageFormat.RAW_SENSOR)
    }

    fun isLogical(ch: CameraCharacteristics) =
        ch[CameraCharacteristics.REQUEST_AVAILABLE_CAPABILITIES]
            ?.contains(CameraMetadata.REQUEST_AVAILABLE_CAPABILITIES_LOGICAL_MULTI_CAMERA) == true

    private fun isBackwardCompatible(ch: CameraCharacteristics) =
        ch[CameraCharacteristics.REQUEST_AVAILABLE_CAPABILITIES]
            ?.contains(CameraMetadata.REQUEST_AVAILABLE_CAPABILITIES_BACKWARD_COMPATIBLE) == true

    /** Cached lenses if this build was scanned before, else null. */
    fun cached(ctx: Context, cm: CameraManager): List<Lens>? {
        val p = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        if (p.getString("fingerprint", null) != Build.FINGERPRINT) return null
        val spec = p.getString("lenses", null) ?: return null
        val all = candidates(cm).associateBy { it.key }
        return spec.split(';').filter { it.isNotBlank() }.mapNotNull { e -> decode(e, all) }
            .let(::finish).takeIf { it.isNotEmpty() }
    }

    /** Catalog entry: key|raw|ispJpeg|WxH|stillWithPreview|previewTemplate|probe marker. */
    private fun encode(l: Lens, marker: String = "") =
        "${l.key}|${b(l.raw)}|${b(l.ispJpeg)}|${l.rawW}x${l.rawH}|${b(l.stillWithPreview)}|${b(l.previewTemplate)}|$marker"

    private fun b(v: Boolean) = if (v) "1" else "0"

    private fun decode(e: String, all: Map<String, Lens>): Lens? {
        val f = e.split('|')
        val base = all[f[0]] ?: return null
        var raw = f.getOrNull(1) == "1"
        // A RAW-capable lens stored as ISP-only was demoted at runtime. Unless the current
        // probe did that, give RAW another chance (the next failure runs the probe).
        if (!raw && hasRaw(base.chars) && f.getOrNull(6) != "p$PROBE_VERSION") raw = true
        val wh = f.getOrNull(3)?.split('x')?.mapNotNull { it.toIntOrNull() }
        return base.copy(raw = raw, ispJpeg = f.getOrNull(2) == "1",
            rawW = wh?.getOrNull(0) ?: 0, rawH = wh?.getOrNull(1) ?: 0,
            stillWithPreview = f.getOrNull(4) == "1", previewTemplate = f.getOrNull(5) == "1")
    }

    /** Stores what was learned about [lens] at runtime; [probed] marks a demotion by the probe. */
    fun store(ctx: Context, lens: Lens, probed: Boolean = false) {
        val p = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val spec = p.getString("lenses", null) ?: return
        val updated = spec.split(';').filter { it.isNotBlank() }.joinToString(";") { e ->
            if (e.substringBefore('|') == lens.key) encode(lens, if (probed) "p$PROBE_VERSION" else "") else e
        }
        p.edit().putString("lenses", updated).apply()
    }

    /** Full scan with verification; [progress] gets (done, total). Stores the result. */
    suspend fun scan(ctx: Context, cm: CameraManager, progress: suspend (Int, Int) -> Unit): List<Lens> {
        val cands = candidates(cm)
        val ok = mutableListOf<Lens>()
        for ((i, l) in cands.withIndex()) {
            progress(i, cands.size)
            verify(cm, l)?.let { ok += it }
        }
        progress(cands.size, cands.size)
        val lenses = finish(dedupe(ok))
        ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit()
            .putString("fingerprint", Build.FINGERPRINT)
            .putString("lenses", lenses.joinToString(";") { encode(it) })
            .apply()
        Log.i(TAG, "scan: " + lenses.joinToString { "${it.label}=${it.key}${if (it.raw) "" else "(ISP)"}${if (it.ispJpeg) "" else "(noJPEG)"}" })
        return lenses
    }

    /** Every lens the framework will describe, deduplicated by sensor. Not yet opened. */
    fun candidates(cm: CameraManager): List<Lens> {
        val listed = runCatching { cm.cameraIdList.toSet() }.getOrDefault(emptySet())
        val all = (listed + (0..MAX_ID).map { it.toString() }).distinct().mapNotNull { id ->
            runCatching { id to cm.getCameraCharacteristics(id) }.getOrNull()
        }
        val out = mutableListOf<Lens>()
        for ((id, ch) in all) {
            if (!isBackwardCompatible(ch)) continue // depth / IR / external helpers
            val logical = isLogical(ch)
            if (logical && id !in listed) continue
            val raw = hasRaw(ch)
            if (raw || id in listed) out += lens(id, null, id in listed, raw, ch)
            if (logical) {
                for (p in ch.physicalCameraIds) {
                    val pch = runCatching { cm.getCameraCharacteristics(p) }.getOrNull() ?: continue
                    if (hasRaw(pch)) out += lens(id, p, false, true, pch)
                }
            }
        }
        return dedupe(out)
    }

    private fun lens(cameraId: String, physicalId: String?, listed: Boolean, raw: Boolean, ch: CameraCharacteristics) =
        Lens(cameraId, physicalId, ch[CameraCharacteristics.LENS_FACING] == CameraMetadata.LENS_FACING_FRONT,
            eqMm(ch), listed, raw, ispJpeg = true, ch)

    /** One lens per sensor: RAW over ISP-only, direct ID over physical stream, non-logical first. */
    private fun dedupe(lenses: List<Lens>): List<Lens> =
        lenses.groupBy { sensorKey(it.chars) }.map { (_, same) ->
            same.sortedWith(compareBy(
                { !it.raw }, { it.physicalId != null }, { isLogical(it.chars) },
                { it.cameraId.toIntOrNull() ?: Int.MAX_VALUE },
            )).first()
        }

    /** Order (rear by focal length, then front) and labels (front lenses get mm when several). */
    private fun finish(lenses: List<Lens>): List<Lens> {
        val sorted = lenses.sortedWith(compareBy({ it.front }, { it.eqMm }))
        val fronts = sorted.count { it.front }
        for (l in sorted) {
            l.label = (if (l.front) (if (fronts > 1) "front ${l.eqMm}" else "front") else "${l.eqMm}mm") + if (l.raw) "" else " ·ISP"
        }
        return sorted
    }

    /**
     * Opens the lens and configures a session as CaptureSession would. Returns the lens with
     * the stream set that worked, or null.
     */
    private suspend fun verify(cm: CameraManager, l: Lens): Lens? {
        val thread = HandlerThread("verify-${l.key}").apply { start() }
        val handler = Handler(thread.looper)
        val exec = Executor { handler.post(it) }
        var device: CameraDevice? = null
        val texture = SurfaceTexture(0)
        try {
            device = withTimeout(4_000) { cm.openAwait(l.cameraId, handler) }
            val preview = CaptureSession.previewSize(l)
            texture.setDefaultBufferSize(preview.width, preview.height)
            val previewSurface = Surface(texture)
            for (withJpeg in listOf(true, false)) {
                if (!withJpeg && !l.raw) return null // ISP-only lens without a JPEG stream: useless
                val readers = CaptureSession.readers(l, withJpeg)
                try {
                    val outs = (listOf(previewSurface) + readers.map { it.surface }).map { s ->
                        OutputConfiguration(s).apply { l.physicalId?.let { setPhysicalCameraId(it) } }
                    }
                    val s = withTimeout(4_000) { device.createSessionAwait(outs, exec) }
                    s.close()
                    return l.copy(ispJpeg = withJpeg).also { it.label = l.label }
                } catch (e: Throwable) {
                    Log.i(TAG, "verify ${l.key} jpeg=$withJpeg: ${e.message}")
                } finally {
                    readers.forEach(ImageReader::close)
                }
            }
            return null
        } catch (e: Throwable) {
            Log.i(TAG, "verify ${l.key}: ${e.message}")
            return null
        } finally {
            runCatching { device?.close() }
            texture.release()
            thread.quitSafely()
        }
    }

    private fun sensorKey(ch: CameraCharacteristics): String {
        val s = ch[CameraCharacteristics.SENSOR_INFO_PHYSICAL_SIZE]
        val px = ch[CameraCharacteristics.SENSOR_INFO_PIXEL_ARRAY_SIZE]
        val f = ch[CameraCharacteristics.LENS_INFO_AVAILABLE_FOCAL_LENGTHS]?.firstOrNull()
        return "${ch[CameraCharacteristics.LENS_FACING]}/$f/${s?.width}x${s?.height}/$px"
    }

    private fun eqMm(ch: CameraCharacteristics): Int {
        val f = ch[CameraCharacteristics.LENS_INFO_AVAILABLE_FOCAL_LENGTHS]?.firstOrNull() ?: return 0
        val s = ch[CameraCharacteristics.SENSOR_INFO_PHYSICAL_SIZE] ?: return 0
        // 35 mm full frame diagonal = 43.27 mm.
        return (f * 43.27 / hypot(s.width.toDouble(), s.height.toDouble())).roundToInt()
    }
}
