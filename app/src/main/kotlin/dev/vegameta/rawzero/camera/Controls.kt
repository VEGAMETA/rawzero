package dev.vegameta.rawzero.camera

import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraMetadata
import android.hardware.camera2.CaptureRequest
import android.hardware.camera2.CaptureResult
import android.hardware.camera2.params.ColorSpaceTransform
import android.hardware.camera2.params.RggbChannelVector
import dev.vegameta.rawzero.Native
import kotlin.math.roundToInt

/** Manual settings; null = auto. Everything is auto by default. */
data class Controls(
    /** Exposure compensation in AE steps (only meaningful while exposure is auto). */
    val evSteps: Int = 0,
    val exposureNs: Long? = null,
    val iso: Int? = null,
    /** Diopters (1 / meters); 0 = infinity. */
    val focusDiopters: Float? = null,
    val kelvin: Int? = null,
    /** Center zoom (1 = full frame). Preview/ISP via CONTROL_ZOOM_RATIO; RAW is cropped by us. */
    val zoom: Float = 1f,
    val flash: Flash = Flash.OFF,
) {
    val exposureManual get() = exposureNs != null || iso != null
}

/** OFF; CAPTURE: torch only around the still (AE meters with it); ALWAYS: torch on. */
enum class Flash { OFF, CAPTURE, ALWAYS }

/** Zoom limits of a lens: (max, uses CONTROL_ZOOM_RATIO). */
fun zoomRange(ch: CameraCharacteristics): Pair<Float, Boolean> {
    ch[CameraCharacteristics.CONTROL_ZOOM_RATIO_RANGE]?.let { return it.upper.coerceAtLeast(1f) to true }
    return (ch[CameraCharacteristics.SCALER_AVAILABLE_MAX_DIGITAL_ZOOM] ?: 1f).coerceAtLeast(1f) to false
}

fun hasFlash(ch: CameraCharacteristics) = ch[CameraCharacteristics.FLASH_INFO_AVAILABLE] == true

/** Longest exposure the preview runs at; longer manual exposures are simulated by ISO. */
const val PREVIEW_MAX_EXPOSURE_NS = 33_333_333L
/** Longest shutter offered in the UI (each lens is further limited by its sensor). */
const val UI_MAX_EXPOSURE_NS = 60_000_000_000L

/**
 * Qualcomm vendor tags (org.codeaurora.qcamera3.iso_exp_priority, Snapdragon HALs incl.
 * Xiaomi 14): exposure-time priority reaches past SENSOR_INFO_EXPOSURE_TIME_RANGE
 * (1 s standard vs 30..201 s here). Only used when the lens advertises the range.
 */
object QcomExposure {
    val RANGE = CameraCharacteristics.Key("org.codeaurora.qcamera3.iso_exp_priority.exposure_time_range", LongArray::class.java)
    /** 0 = ISO priority, 1 = exposure time priority. */
    val SELECT_PRIORITY = CaptureRequest.Key("org.codeaurora.qcamera3.iso_exp_priority.select_priority", Int::class.javaObjectType)
    /** Exposure time in ns when SELECT_PRIORITY = 1. */
    val VALUE = CaptureRequest.Key("org.codeaurora.qcamera3.iso_exp_priority.use_iso_exp_priority", Long::class.javaObjectType)

    fun max(ch: CameraCharacteristics): Long? = runCatching { ch.get(RANGE)?.getOrNull(1) }.getOrNull()}

/** Longest exposure a still capture can use on this lens. */
fun maxStillExposure(ch: CameraCharacteristics): Long {
    val std = ch[CameraCharacteristics.SENSOR_INFO_EXPOSURE_TIME_RANGE]?.upper ?: 1_000_000_000L
    return maxOf(std, QcomExposure.max(ch) ?: 0L)
}

/** Per-lens option lists for the settings sliders. Index 0 of each list is "auto". */
class LensOptions(val lens: Lens) {
    private val ch = lens.chars
    val evStep: Float = ch[CameraCharacteristics.CONTROL_AE_COMPENSATION_STEP]?.toFloat() ?: (1f / 3)
    val evRange: IntRange = ch[CameraCharacteristics.CONTROL_AE_COMPENSATION_RANGE]?.let { it.lower..it.upper } ?: 0..0

    private val expMin = ch[CameraCharacteristics.SENSOR_INFO_EXPOSURE_TIME_RANGE]?.lower ?: 0L
    private val expMax = minOf(maxStillExposure(ch), UI_MAX_EXPOSURE_NS)
    val exposures: List<Long?> = listOf<Long?>(null) + listOf(
        1.0 / 8000, 1.0 / 4000, 1.0 / 2000, 1.0 / 1000, 1.0 / 500, 1.0 / 250, 1.0 / 125, 1.0 / 60,
        1.0 / 30, 1.0 / 15, 1.0 / 8, 1.0 / 4, 1.0 / 2, 1.0, 2.0, 4.0, 8.0, 10.0, 15.0, 20.0, 30.0, 45.0, 60.0,
    ).map { (it * 1e9).toLong() }.filter { it in expMin..expMax }.let { l ->
        // The sensor's own limit if it falls between stops (Xiaomi front: 0.41 s).
        if (l.isNotEmpty() && expMax > l.last() * 1.15) l + expMax else l
    }

    private val isoRange = ch[CameraCharacteristics.SENSOR_INFO_SENSITIVITY_RANGE]
    val isos: List<Int?> = listOf<Int?>(null) + listOf(50, 100, 200, 400, 800, 1600, 3200, 6400, 12800)
        .filter { isoRange == null || it in isoRange.lower..isoRange.upper }.let { l ->
            // The sensor's top ISO if it falls between stops (Xiaomi W: 3188).
            val top = isoRange?.upper
            if (top != null && l.isNotEmpty() && top > l.last() * 1.15) l + top else l
        }

    /** Closest focus in diopters; 0 means fixed focus (front cameras). */
    val minFocus: Float = ch[CameraCharacteristics.LENS_INFO_MINIMUM_FOCUS_DISTANCE] ?: 0f
    val focusSteps = 40
    val kelvins: List<Int?> = listOf<Int?>(null) + (2500..8000 step 250).toList()

    fun focusAt(i: Int): Float? = if (i == 0) null else minFocus * (i - 1) / (focusSteps - 1)
}

fun formatExposure(ns: Long): String = when {
    ns >= 1_000_000_000L -> "${(ns / 1e9).roundToInt()} с"
    ns >= 400_000_000L -> "%.1f с".format(ns / 1e9)
    else -> "1/${(1e9 / ns).roundToInt()}"
}

fun formatFocus(d: Float): String = if (d <= 0.001f) "∞" else if (1 / d >= 1) "%.1f м".format(1 / d) else "${(100 / d).roundToInt()} см"

/**
 * Applies [c] to a request. Manual exposure needs both time and ISO; the missing one is
 * derived from the last auto-metered frame so brightness stays put (time x ISO constant).
 *
 * Preview never runs a long exposure: the sensor delivers at most one frame per exposure,
 * so at 1 s the viewfinder would update once a second. Exposures beyond
 * [PREVIEW_MAX_EXPOSURE_NS] are previewed at that time with ISO scaled up by the same
 * factor (same brightness, as far as the ISO range allows); the still gets the real value,
 * past the standard limit through [QcomExposure] where available.
 *
 * Manual WB sets COLOR_CORRECTION_GAINS from the sensor calibration at that temperature
 * and keeps the last auto color transform for the ISP preview.
 */
fun CaptureRequest.Builder.applyControls(
    lens: Lens, c: Controls, lastAutoAe: CaptureResult?, lastAutoAwb: CaptureResult?, preview: Boolean,
) {
    val ch = lens.chars
    set(CaptureRequest.CONTROL_MODE, CameraMetadata.CONTROL_MODE_AUTO)
    // Qualcomm vendor tags are sticky in the HAL: a long-exposure still would keep the
    // preview at that exposure. Every request resets them unless it sets them itself.
    if (QcomExposure.max(ch) != null) {
        set(QcomExposure.SELECT_PRIORITY, 0)
        set(QcomExposure.VALUE, 0L)
    }
    // ISP-only gain is used solely to preview long exposures; 1x everywhere else.
    if (ch[CameraCharacteristics.CONTROL_POST_RAW_SENSITIVITY_BOOST_RANGE] != null) {
        set(CaptureRequest.CONTROL_POST_RAW_SENSITIVITY_BOOST, 100)
    }
    if (c.exposureManual) {
        val meteredExp = lastAutoAe?.get(CaptureResult.SENSOR_EXPOSURE_TIME) ?: 10_000_000L
        val meteredIso = lastAutoAe?.get(CaptureResult.SENSOR_SENSITIVITY) ?: 400
        val isoRange = ch[CameraCharacteristics.SENSOR_INFO_SENSITIVITY_RANGE]
        val expRange = ch[CameraCharacteristics.SENSOR_INFO_EXPOSURE_TIME_RANGE]
        val exp = c.exposureNs ?: (meteredExp.toDouble() * meteredIso / c.iso!!).toLong()
        val iso = c.iso ?: (meteredIso.toDouble() * meteredExp / exp).roundToInt()
        val clampIso = { v: Int -> isoRange?.clamp(v) ?: v }
        set(CaptureRequest.CONTROL_AE_MODE, CameraMetadata.CONTROL_AE_MODE_OFF)
        when {
            preview && exp > PREVIEW_MAX_EXPOSURE_NS -> {
                // Show what the still will really get: exposure x ISO *after* the still's ISO
                // clamp (shutter-only at 2 s indoors needs ISO ~13, the sensor floor is 50, so
                // the photo is brighter than "auto"). Sensor ISO first, the rest as ISP gain on
                // the processed preview stream only (POST_RAW_SENSITIVITY_BOOST, 100 = 1x).
                val product = exp.toDouble() * clampIso(iso)
                val wantIso = product / PREVIEW_MAX_EXPOSURE_NS
                val sensorIso = clampIso(wantIso.roundToInt())
                set(CaptureRequest.SENSOR_EXPOSURE_TIME, PREVIEW_MAX_EXPOSURE_NS)
                set(CaptureRequest.SENSOR_SENSITIVITY, sensorIso)
                ch[CameraCharacteristics.CONTROL_POST_RAW_SENSITIVITY_BOOST_RANGE]?.let { r ->
                    set(CaptureRequest.CONTROL_POST_RAW_SENSITIVITY_BOOST, r.clamp((100 * wantIso / sensorIso).roundToInt()))
                }
            }
            !preview && expRange != null && exp > expRange.upper && (QcomExposure.max(ch) ?: 0L) >= exp -> {
                // Measured on Xiaomi 14 (2026-11-06): capture time matches the exposure and
                // RAW signal grows with it (2 s -> 1.6x, 8 s -> 3.3x of 1 s), but the HAL picks
                // the ISO itself (requested 100 -> 82 / 50). AE-on and no-exposure-key variants
                // behaved the same.
                set(QcomExposure.SELECT_PRIORITY, 1)
                set(QcomExposure.VALUE, exp)
                set(CaptureRequest.SENSOR_EXPOSURE_TIME, exp)
                set(CaptureRequest.SENSOR_FRAME_DURATION, exp)
                set(CaptureRequest.SENSOR_SENSITIVITY, clampIso(iso))
            }
            else -> {
                set(CaptureRequest.SENSOR_EXPOSURE_TIME, expRange?.clamp(exp) ?: exp)
                set(CaptureRequest.SENSOR_SENSITIVITY, clampIso(iso))
            }
        }
    } else {
        set(CaptureRequest.CONTROL_AE_MODE, CameraMetadata.CONTROL_AE_MODE_ON)
        set(CaptureRequest.CONTROL_AE_EXPOSURE_COMPENSATION, c.evSteps)
    }

    val af = ch[CameraCharacteristics.CONTROL_AF_AVAILABLE_MODES] ?: intArrayOf()
    if (c.focusDiopters != null) {
        set(CaptureRequest.CONTROL_AF_MODE, CameraMetadata.CONTROL_AF_MODE_OFF)
        set(CaptureRequest.LENS_FOCUS_DISTANCE, c.focusDiopters)
    } else if (CameraMetadata.CONTROL_AF_MODE_CONTINUOUS_PICTURE in af) {
        set(CaptureRequest.CONTROL_AF_MODE, CameraMetadata.CONTROL_AF_MODE_CONTINUOUS_PICTURE)
    }

    // Zoom: the ISP outputs (preview, JPEG) are cropped by the HAL; RAW is never cropped by
    // Camera2, the pipeline crops it to the same center fraction (Native.arrays).
    val (zmax, ratio) = zoomRange(ch)
    val z = c.zoom.coerceIn(1f, zmax)
    if (ratio) {
        set(CaptureRequest.CONTROL_ZOOM_RATIO, z)
    } else if (z > 1f) {
        ch[CameraCharacteristics.SENSOR_INFO_ACTIVE_ARRAY_SIZE]?.let { a ->
            val w = (a.width() / z).toInt()
            val h = (a.height() / z).toInt()
            val l = (a.width() - w) / 2
            val t = (a.height() - h) / 2
            set(CaptureRequest.SCALER_CROP_REGION, android.graphics.Rect(l, t, l + w, t + h))
        }
    }
    if (hasFlash(ch)) {
        val torch = c.flash == Flash.ALWAYS || (c.flash == Flash.CAPTURE && !preview)
        set(CaptureRequest.FLASH_MODE, if (torch) CameraMetadata.FLASH_MODE_TORCH else CameraMetadata.FLASH_MODE_OFF)
    }

    if (c.kelvin != null) {
        val g = Native.wbGainsFor(ch, c.kelvin.toDouble())
        set(CaptureRequest.CONTROL_AWB_MODE, CameraMetadata.CONTROL_AWB_MODE_OFF)
        set(CaptureRequest.COLOR_CORRECTION_MODE, CameraMetadata.COLOR_CORRECTION_MODE_TRANSFORM_MATRIX)
        set(CaptureRequest.COLOR_CORRECTION_GAINS, RggbChannelVector(g[0], g[1], g[2], g[3]))
        set(CaptureRequest.COLOR_CORRECTION_TRANSFORM,
            lastAutoAwb?.get(CaptureResult.COLOR_CORRECTION_TRANSFORM) ?: identityTransform)
    } else {
        set(CaptureRequest.CONTROL_AWB_MODE, CameraMetadata.CONTROL_AWB_MODE_AUTO)
    }
}

private val identityTransform = ColorSpaceTransform(intArrayOf(1, 1, 0, 1, 0, 1, 0, 1, 1, 1, 0, 1, 0, 1, 0, 1, 1, 1))
