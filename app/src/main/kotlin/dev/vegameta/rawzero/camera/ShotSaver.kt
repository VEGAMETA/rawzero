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
 * Gallery layout:
 *   Pictures/RawZero/<name>.jpg          our pipeline, the photo
 *   Pictures/RawZero/DNG/<name>.dng      optional RAW
 *   Pictures/RawZero/dev/<name>.dng + <name>_isp.jpg   look-development pairs (dev toggle)
 */
object ShotSaver {
    const val DIR = "Pictures/RawZero"
    const val DIR_DNG = "Pictures/RawZero/DNG"
    const val DIR_DEV = "Pictures/RawZero/dev"

    fun baseName(shot: Shot): String =
        "RZ_" + SimpleDateFormat("yyyyMMdd_HHmmss_SSS", Locale.US).format(Date()) + "_cam" + shot.lens.id

    fun saveJpeg(cr: ContentResolver, dir: String, name: String, bytes: ByteArray) =
        write(cr, dir, "$name.jpg", "image/jpeg") { it.write(bytes) }

    fun saveDng(cr: ContentResolver, dir: String, name: String, shot: Shot): Uri {
        val raw = requireNotNull(shot.raw) { "no RAW for ${shot.lens.key}" }
        // A physical stream's result can lack keys DngCreator wants; the logical one has them.
        val creator = runCatching { DngCreator(shot.lens.chars, shot.result) }.getOrElse { DngCreator(shot.lens.chars, shot.total) }
        return write(cr, dir, "$name.dng", "image/x-adobe-dng") { out ->
            creator.use { d ->
                d.setOrientation(exifOrientation(shot.jpegOrientation))
                d.writeImage(out, raw)
            }
        }
    }

    private fun write(cr: ContentResolver, dir: String, file: String, mime: String, body: (OutputStream) -> Unit): Uri {
        val values = ContentValues().apply {
            put(MediaStore.MediaColumns.DISPLAY_NAME, file)
            put(MediaStore.MediaColumns.MIME_TYPE, mime)
            put(MediaStore.MediaColumns.RELATIVE_PATH, dir)
            put(MediaStore.MediaColumns.IS_PENDING, 1)
        }
        val uri: Uri = cr.insert(MediaStore.Images.Media.getContentUri(MediaStore.VOLUME_EXTERNAL_PRIMARY), values)
            ?: error("MediaStore insert failed for $file")
        try {
            cr.openOutputStream(uri)!!.use(body)
            cr.update(uri, ContentValues().apply { put(MediaStore.MediaColumns.IS_PENDING, 0) }, null, null)
        } catch (e: Throwable) {
            cr.delete(uri, null, null)
            throw e
        }
        return uri
    }

    /** Newest photo of ours in the gallery (only files this install created are visible). */
    fun latest(cr: ContentResolver): Uri? {
        val collection = MediaStore.Images.Media.getContentUri(MediaStore.VOLUME_EXTERNAL_PRIMARY)
        cr.query(
            collection, arrayOf(MediaStore.MediaColumns._ID),
            "${MediaStore.MediaColumns.RELATIVE_PATH} = ? AND ${MediaStore.MediaColumns.MIME_TYPE} = ?",
            arrayOf("$DIR/", "image/jpeg"), "${MediaStore.MediaColumns.DATE_ADDED} DESC",
        )?.use { c -> if (c.moveToFirst()) return android.content.ContentUris.withAppendedId(collection, c.getLong(0)) }
        return null
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
        val exp = r[CaptureResult.SENSOR_EXPOSURE_TIME]?.let { formatExposure(it) }
        return "ISO $iso  $exp"
    }
}
