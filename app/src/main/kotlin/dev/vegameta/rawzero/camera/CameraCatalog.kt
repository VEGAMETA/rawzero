package dev.vegameta.rawzero.camera

import android.graphics.ImageFormat
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CameraMetadata
import kotlin.math.hypot
import kotlin.math.roundToInt

/** One physical lens the app can shoot RAW with. */
data class Lens(
    val id: String,
    val front: Boolean,
    /** 35 mm equivalent focal length (diagonal crop factor). */
    val eqMm: Int,
    val listed: Boolean,
    val chars: CameraCharacteristics,
) {
    val label: String get() = if (front) "front" else "${eqMm}mm"
}

/**
 * Discovers every RAW-capable camera, including ones `getCameraIdList()` omits
 * (Xiaomi: hidden physical IDs; Samsung: allow-listed package names, see CLAUDE.md).
 * Hidden logical cameras are never touched (opening Xiaomi's logical ID 5 crashed the
 * camera provider). Duplicates of the same sensor (logical wrappers, Samsung 0/56) are
 * collapsed to the lowest ID that is not a logical camera.
 */
object CameraCatalog {
    private const val MAX_ID = 127

    fun discover(cm: CameraManager): List<Lens> {
        val listed = cm.cameraIdList.toSet()
        val all = (listed + (0..MAX_ID).map { it.toString() }).distinct().mapNotNull { id ->
            runCatching { id to cm.getCameraCharacteristics(id) }.getOrNull()
        }
        val usable = all.filter { (id, ch) -> hasRaw(ch) && !(isLogical(ch) && id !in listed) }
        return usable
            .groupBy { (_, ch) -> sensorKey(ch) }
            .map { (_, same) ->
                same.sortedWith(compareBy({ isLogical(it.second) }, { it.first.toIntOrNull() ?: Int.MAX_VALUE })).first()
            }
            .map { (id, ch) -> Lens(id, ch[CameraCharacteristics.LENS_FACING] == CameraMetadata.LENS_FACING_FRONT, eqMm(ch), id in listed, ch) }
            .sortedWith(compareBy({ it.front }, { it.eqMm }))
    }

    fun hasRaw(ch: CameraCharacteristics): Boolean {
        val caps = ch[CameraCharacteristics.REQUEST_AVAILABLE_CAPABILITIES] ?: return false
        val map = ch[CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP] ?: return false
        return CameraMetadata.REQUEST_AVAILABLE_CAPABILITIES_RAW in caps && map.outputFormats.contains(ImageFormat.RAW_SENSOR)
    }

    fun isLogical(ch: CameraCharacteristics) =
        ch[CameraCharacteristics.REQUEST_AVAILABLE_CAPABILITIES]
            ?.contains(CameraMetadata.REQUEST_AVAILABLE_CAPABILITIES_LOGICAL_MULTI_CAMERA) == true

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
