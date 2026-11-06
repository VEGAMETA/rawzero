package dev.vegameta.rawzero

import android.graphics.ImageFormat
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraMetadata
import android.hardware.camera2.CaptureResult
import android.hardware.camera2.params.ColorSpaceTransform
import android.media.Image
import android.os.Build

/** Rust pipeline (jni-bridge). Array layouts must match `jni-bridge/src/lib.rs`. */
object Native {
    init {
        System.loadLibrary("rawzero_jni")
    }

    @JvmStatic
    external fun processRaw16(
        raw: java.nio.ByteBuffer, width: Int, height: Int, rowStride: Int,
        ints: IntArray, floats: FloatArray, doubles: DoubleArray, shading: FloatArray?, make: String,
        reference: ByteArray?, refWidth: Int, refHeight: Int,
    ): ByteArray

    /**
     * ISP JPEG of the same request, decoded at 1/4 size to RGB8: the look reference for
     * Classic (only its brightness distribution is used, so the size does not matter).
     */
    fun referenceRgb(ispJpeg: ByteArray): Triple<ByteArray, Int, Int>? {
        val bmp = android.graphics.BitmapFactory.decodeByteArray(ispJpeg, 0, ispJpeg.size,
            android.graphics.BitmapFactory.Options().apply { inSampleSize = 4 }) ?: return null
        return referenceRgb(bmp).also { bmp.recycle() }
    }

    /** Any ISP-rendered bitmap (e.g. the preview frame at the shutter) as RGB8. */
    fun referenceRgb(bmp: android.graphics.Bitmap): Triple<ByteArray, Int, Int> {
        val px = IntArray(bmp.width * bmp.height)
        bmp.getPixels(px, 0, bmp.width, 0, 0, bmp.width, bmp.height)
        val rgb = ByteArray(px.size * 3)
        for (i in px.indices) {
            val c = px[i]
            rgb[3 * i] = (c shr 16).toByte()
            rgb[3 * i + 1] = (c shr 8).toByte()
            rgb[3 * i + 2] = c.toByte()
        }
        return Triple(rgb, bmp.width, bmp.height)
    }

    @JvmStatic
    external fun wbGains(ints: IntArray, doubles: DoubleArray, kelvin: Double): FloatArray

    /** COLOR_CORRECTION_GAINS for a Planckian white at [kelvin], from the sensor's color calibration. */
    fun wbGainsFor(ch: CameraCharacteristics, kelvin: Double): FloatArray {
        val ints = IntArray(INTS_LEN)
        ints[ILLUMINANT1] = ch[CameraCharacteristics.SENSOR_REFERENCE_ILLUMINANT1] ?: 21
        ints[ILLUMINANT2] = ch[CameraCharacteristics.SENSOR_REFERENCE_ILLUMINANT2]?.toInt() ?: 0
        return wbGains(ints, colorMatrices(ch), kelvin)
    }

    private fun colorMatrices(ch: CameraCharacteristics): DoubleArray {
        val doubles = DoubleArray(54) { Double.NaN }
        listOf(
            ch[CameraCharacteristics.SENSOR_COLOR_TRANSFORM1], ch[CameraCharacteristics.SENSOR_COLOR_TRANSFORM2],
            ch[CameraCharacteristics.SENSOR_FORWARD_MATRIX1], ch[CameraCharacteristics.SENSOR_FORWARD_MATRIX2],
            ch[CameraCharacteristics.SENSOR_CALIBRATION_TRANSFORM1], ch[CameraCharacteristics.SENSOR_CALIBRATION_TRANSFORM2],
        ).forEachIndexed { m, t -> t?.let { put(doubles, m, it) } }
        return doubles
    }

    // ints layout
    private const val CFA = 0
    private const val ORIENTATION = 1
    private const val HOT_PIXELS_CORRECTED = 2
    private const val ILLUMINANT1 = 3
    private const val ILLUMINANT2 = 4
    private const val HAL_SHADING_APPLIED = 5
    private const val SHADING_ROWS = 6
    private const val SHADING_COLS = 7
    private const val JPEG_QUALITY = 8
    private const val CROP_X = 9
    private const val CROP_Y = 10
    private const val CROP_W = 11
    private const val CROP_H = 12
    private const val MIRROR = 13
    private const val MODE = 14
    private const val REF_ROTATE_CW = 15
    private const val REF_MIRROR = 16
    private const val STACK = 17
    private const val INTS_LEN = 18

    /** Fills FrameMeta from CameraCharacteristics + CaptureResult and runs the pipeline. */
    /**
     * [mirrorDegrees]: null = no mirror; otherwise the JPEG rotation in degrees, and the photo
     * is flipped left-right as displayed (selfie preview look). With a 90/270 rotation the
     * displayed horizontal axis is the stored vertical one, so rows are reversed.
     */
    fun process(raw: Image, ch: CameraCharacteristics, r: CaptureResult, exifOrientation: Int,
                mirrorDegrees: Int? = null, reference: Triple<ByteArray, Int, Int>? = null,
                classic: Boolean = false, refRotateCw: Int = 0, refMirror: Boolean = false, quality: Int = 92, zoom: Float = 1f): ByteArray {
        val a = arrays(raw, ch, r, exifOrientation, mirrorDegrees, classic, refRotateCw, refMirror, quality, zoom)
        val plane = raw.planes[0]
        return processRaw16(plane.buffer, raw.width, raw.height, plane.rowStride, a.ints, a.floats, a.doubles, a.shading, Build.MANUFACTURER,
            reference?.first, reference?.second ?: 0, reference?.third ?: 0)
    }

    @JvmStatic external fun nightBegin(raw: java.nio.ByteBuffer, width: Int, height: Int, rowStride: Int,
        ints: IntArray, floats: FloatArray, doubles: DoubleArray, shading: FloatArray?, make: String): Long
    @JvmStatic external fun nightAdd(handle: Long, raw: java.nio.ByteBuffer, width: Int, height: Int, rowStride: Int): Int
    @JvmStatic external fun nightFinish(handle: Long, reference: ByteArray?, refWidth: Int, refHeight: Int): ByteArray
    @JvmStatic external fun nightCancel(handle: Long)

    /**
     * Night burst (HDR+-style align + merge in Rust, then Classic). Frames are fed as they
     * arrive and closed by the caller right after [add]; only the reference and accumulators
     * live in native memory.
     */
    class NightBurst(first: Image, ch: CameraCharacteristics, r: CaptureResult, exifOrientation: Int,
                     mirrorDegrees: Int?, refRotateCw: Int, refMirror: Boolean, zoom: Float = 1f,
                     /** true: long-exposure sum instead of night align+merge. */
                     stack: Boolean = false, classic: Boolean = true) : AutoCloseable {
        private var handle: Long

        init {
            val a = arrays(first, ch, r, exifOrientation, mirrorDegrees, classic, refRotateCw, refMirror, 92, zoom)
            a.ints[STACK] = if (stack) 1 else 0
            val p = first.planes[0]
            handle = nightBegin(p.buffer, first.width, first.height, p.rowStride, a.ints, a.floats, a.doubles, a.shading, Build.MANUFACTURER)
        }

        fun add(frame: Image): Int {
            val p = frame.planes[0]
            return nightAdd(handle, p.buffer, frame.width, frame.height, p.rowStride)
        }

        fun finish(reference: Triple<ByteArray, Int, Int>?): ByteArray {
            val h = handle
            handle = 0
            return nightFinish(h, reference?.first, reference?.second ?: 0, reference?.third ?: 0)
        }

        override fun close() {
            if (handle != 0L) nightCancel(handle)
            handle = 0
        }
    }

    private class Arrays(val ints: IntArray, val floats: FloatArray, val doubles: DoubleArray, val shading: FloatArray?)

    /** FrameMeta as flat arrays (layout in jni-bridge/src/lib.rs). */
    private fun arrays(raw: Image, ch: CameraCharacteristics, r: CaptureResult, exifOrientation: Int, mirrorDegrees: Int?,
                       classic: Boolean, refRotateCw: Int, refMirror: Boolean, quality: Int, zoom: Float = 1f): Arrays {
        require(raw.format == ImageFormat.RAW_SENSOR) { "format ${raw.format}" }
        val ints = IntArray(INTS_LEN)
        ints[CFA] = ch[CameraCharacteristics.SENSOR_INFO_COLOR_FILTER_ARRANGEMENT] ?: error("no CFA arrangement")
        ints[ORIENTATION] = exifOrientation
        ints[HOT_PIXELS_CORRECTED] = if ((r[CaptureResult.HOT_PIXEL_MODE] ?: CameraMetadata.HOT_PIXEL_MODE_FAST) != CameraMetadata.HOT_PIXEL_MODE_OFF) 1 else 0
        ints[ILLUMINANT1] = ch[CameraCharacteristics.SENSOR_REFERENCE_ILLUMINANT1] ?: 21
        ints[ILLUMINANT2] = ch[CameraCharacteristics.SENSOR_REFERENCE_ILLUMINANT2]?.toInt() ?: 0
        ints[HAL_SHADING_APPLIED] = if (ch[CameraCharacteristics.SENSOR_INFO_LENS_SHADING_APPLIED] == true) 1 else 0
        ints[JPEG_QUALITY] = quality
        ints[MODE] = if (classic) 1 else 0
        ints[REF_ROTATE_CW] = refRotateCw
        ints[REF_MIRROR] = if (refMirror) 1 else 0
        ints[MIRROR] = when (mirrorDegrees) {
            null -> 0
            else -> if (mirrorDegrees % 180 == 0) 1 else 2
        }

        val lsm = r[CaptureResult.STATISTICS_LENS_SHADING_CORRECTION_MAP]
        val shading = lsm?.let { m ->
            ints[SHADING_ROWS] = m.rowCount
            ints[SHADING_COLS] = m.columnCount
            FloatArray(m.rowCount * m.columnCount * 4).also { out ->
                var k = 0
                for (row in 0 until m.rowCount) for (col in 0 until m.columnCount) for (c in 0 until 4) {
                    out[k++] = m.getGainFactor(c, col, row)
                }
            }
        }

        // Crop to the pre-correction active array when the RAW covers the full pixel array,
        // then to the zoomed center (RAW streams are never cropped by CONTROL_ZOOM_RATIO).
        val pixelArray = ch[CameraCharacteristics.SENSOR_INFO_PIXEL_ARRAY_SIZE]
        val active = ch[CameraCharacteristics.SENSOR_INFO_PRE_CORRECTION_ACTIVE_ARRAY_SIZE]
        var cx = 0; var cy = 0; var cw = raw.width; var chh = raw.height
        if (pixelArray != null && active != null && raw.width == pixelArray.width && raw.height == pixelArray.height &&
            (active.width() < raw.width || active.height() < raw.height)) {
            cx = active.left; cy = active.top; cw = active.width(); chh = active.height()
        }
        if (zoom > 1.001f) {
            val w = (cw / zoom).toInt() and 1.inv()
            val h = (chh / zoom).toInt() and 1.inv()
            cx += ((cw - w) / 2) and 1.inv(); cy += ((chh - h) / 2) and 1.inv(); cw = w; chh = h
        }
        if (cw < raw.width || chh < raw.height) {
            ints[CROP_X] = cx; ints[CROP_Y] = cy; ints[CROP_W] = cw; ints[CROP_H] = chh
        }

        val floats = FloatArray(9)
        val dyn = r[CaptureResult.SENSOR_DYNAMIC_BLACK_LEVEL]
        val pattern = ch[CameraCharacteristics.SENSOR_BLACK_LEVEL_PATTERN]
        for (i in 0 until 4) {
            floats[i] = dyn?.get(i) ?: pattern?.getOffsetForIndex(i % 2, i / 2)?.toFloat() ?: 0f
        }
        floats[4] = (r[CaptureResult.SENSOR_DYNAMIC_WHITE_LEVEL] ?: ch[CameraCharacteristics.SENSOR_INFO_WHITE_LEVEL] ?: 1023).toFloat()
        val g = r[CaptureResult.COLOR_CORRECTION_GAINS] ?: error("no COLOR_CORRECTION_GAINS")
        floats[5] = g.red; floats[6] = g.greenEven; floats[7] = g.greenOdd; floats[8] = g.blue

        return Arrays(ints, floats, colorMatrices(ch), shading)
    }

    private fun put(out: DoubleArray, m: Int, t: ColorSpaceTransform) {
        for (row in 0 until 3) for (col in 0 until 3) out[9 * m + 3 * row + col] = t.getElement(col, row).toDouble()
    }
}
