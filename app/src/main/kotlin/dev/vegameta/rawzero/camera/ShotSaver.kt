package dev.vegameta.rawzero.camera

import android.content.ContentResolver
import android.content.ContentValues
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CaptureResult
import android.hardware.camera2.DngCreator
import android.media.ExifInterface
import android.net.Uri
import android.provider.MediaStore
import java.io.OutputStream
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/**
 * Writes a shot as a pair with one base name into Pictures/RawZero/dev/:
 * `<time>_<lens>.dng` (DngCreator) and `<time>_<lens>_isp.jpg` (ISP output of the same request).
 */
object ShotSaver {
    const val DIR = "Pictures/RawZero/dev"

    fun save(cr: ContentResolver, shot: Shot): List<String> {
        val base = SimpleDateFormat("yyyyMMdd_HHmmss_SSS", Locale.US).format(Date()) + "_cam" + shot.lens.id
        val dng = "$base.dng"
        val jpg = "${base}_isp.jpg"
        write(cr, dng, "image/x-adobe-dng") { out ->
            DngCreator(shot.lens.chars, shot.result).use { d ->
                d.setOrientation(exifOrientation(shot.jpegOrientation))
                d.writeImage(out, shot.raw)
            }
        }
        write(cr, jpg, "image/jpeg") { it.write(shot.ispJpeg) }
        return listOf(dng, jpg)
    }

    private fun write(cr: ContentResolver, name: String, mime: String, body: (OutputStream) -> Unit) {
        val values = ContentValues().apply {
            put(MediaStore.MediaColumns.DISPLAY_NAME, name)
            put(MediaStore.MediaColumns.MIME_TYPE, mime)
            put(MediaStore.MediaColumns.RELATIVE_PATH, DIR)
            put(MediaStore.MediaColumns.IS_PENDING, 1)
        }
        val uri: Uri = cr.insert(MediaStore.Images.Media.getContentUri(MediaStore.VOLUME_EXTERNAL_PRIMARY), values)
            ?: error("MediaStore insert failed for $name")
        try {
            cr.openOutputStream(uri)!!.use(body)
            cr.update(uri, ContentValues().apply { put(MediaStore.MediaColumns.IS_PENDING, 0) }, null, null)
        } catch (e: Throwable) {
            cr.delete(uri, null, null)
            throw e
        }
    }

    fun exifOrientation(degrees: Int) = when ((degrees % 360 + 360) % 360) {
        90 -> ExifInterface.ORIENTATION_ROTATE_90
        180 -> ExifInterface.ORIENTATION_ROTATE_180
        270 -> ExifInterface.ORIENTATION_ROTATE_270
        else -> ExifInterface.ORIENTATION_NORMAL
    }

    /** Camera2 `JPEG_ORIENTATION` docs: sensor orientation plus device rotation, negated for the front lens. */
    fun jpegOrientation(chars: CameraCharacteristics, deviceDegrees: Int): Int {
        val sensor = chars[CameraCharacteristics.SENSOR_ORIENTATION] ?: 0
        val front = chars[CameraCharacteristics.LENS_FACING] == CameraCharacteristics.LENS_FACING_FRONT
        val dev = (deviceDegrees + 45) / 90 * 90
        return (sensor + (if (front) -dev else dev) + 360) % 360
    }

    fun summary(r: CaptureResult): String {
        val iso = r[CaptureResult.SENSOR_SENSITIVITY]
        val exp = r[CaptureResult.SENSOR_EXPOSURE_TIME]?.let { if (it >= 1_000_000_000L) "%.1fs".format(it / 1e9) else "1/%.0f".format(1e9 / it) }
        return "ISO $iso  $exp"
    }
}
