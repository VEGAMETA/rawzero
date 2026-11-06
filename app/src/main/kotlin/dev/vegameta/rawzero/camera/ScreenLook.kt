package dev.vegameta.rawzero.camera

import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Matrix
import android.media.ExifInterface
import java.io.ByteArrayOutputStream
import java.io.File

/**
 * "Screen" output: the ISP's full-resolution still JPEG (good detail), recolored to what the
 * viewfinder showed. The HAL tunes its preview and still pipelines differently (on Xiaomi 14
 * the still is visibly cooler and flatter than the preview), so the preview frame grabbed at
 * the shutter is the reference: per-channel histogram specification gives three monotone
 * curves mapping the still's R, G, B distributions onto the preview's. Global and monotone,
 * so no halos and no content-dependent local changes; works without pixel registration.
 */
object ScreenLook {

    /** For channel c: lut[c][v] = value whose preview CDF reaches the still's CDF at v. */
    fun fitLuts(still: Bitmap, preview: Bitmap): Array<IntArray> {
        val hs = histograms(still)
        val hp = histograms(preview)
        return Array(3) { c ->
            val cs = cdf(hs[c])
            val cp = cdf(hp[c])
            var u = 0
            IntArray(256) { v ->
                while (u < 255 && cp[u] < cs[v]) u++
                u
            }
        }
    }

    private fun histograms(b: Bitmap): Array<LongArray> {
        val small = if (b.width * b.height > 600_000) Bitmap.createScaledBitmap(b, b.width / 4, b.height / 4, true) else b
        val px = IntArray(small.width * small.height)
        small.getPixels(px, 0, small.width, 0, 0, small.width, small.height)
        if (small !== b) small.recycle()
        val h = Array(3) { LongArray(256) }
        for (p in px) {
            h[0][(p shr 16) and 0xFF]++
            h[1][(p shr 8) and 0xFF]++
            h[2][p and 0xFF]++
        }
        return h
    }

    private fun cdf(h: LongArray): DoubleArray {
        val total = h.sum().toDouble().coerceAtLeast(1.0)
        var acc = 0L
        return DoubleArray(256) { acc += h[it]; acc / total }
    }

    /**
     * Full pipeline for a Screen photo: decode, recolor to [preview] (if any), mirror for the
     * front lens, encode at quality 95 with the EXIF rotation (the Xiaomi HAL writes none).
     */
    fun render(ctx: Context, ispJpeg: ByteArray, preview: Bitmap?, jpegOrientation: Int, mirror: Boolean): ByteArray {
        var bmp = BitmapFactory.decodeByteArray(ispJpeg, 0, ispJpeg.size, BitmapFactory.Options().apply { inMutable = true })
            ?: return ispJpeg
        if (preview != null) {
            val lut = fitLuts(bmp, preview)
            val w = bmp.width
            val row = IntArray(w)
            for (y in 0 until bmp.height) {
                bmp.getPixels(row, 0, w, 0, y, w, 1)
                for (x in 0 until w) {
                    val p = row[x]
                    row[x] = (p and -0x1000000) or
                        (lut[0][(p shr 16) and 0xFF] shl 16) or (lut[1][(p shr 8) and 0xFF] shl 8) or lut[2][p and 0xFF]
                }
                bmp.setPixels(row, 0, w, 0, y, w, 1)
            }
        }
        // Most HALs (Xiaomi) leave pixels in sensor orientation and write no EXIF rotation; a
        // portrait bitmap for a 90/270 request means this HAL rotated the pixels itself.
        val residual = if (jpegOrientation % 180 != 0 && bmp.height > bmp.width) 0 else jpegOrientation
        if (mirror) {
            // With a 90/270 rotation still to apply, the displayed horizontal axis is the
            // stored vertical one.
            val m = Matrix().apply { if (residual % 180 == 0) postScale(-1f, 1f) else postScale(1f, -1f) }
            bmp = Bitmap.createBitmap(bmp, 0, 0, bmp.width, bmp.height, m, true).also { bmp.recycle() }
        }
        val out = ByteArrayOutputStream(ispJpeg.size)
        bmp.compress(Bitmap.CompressFormat.JPEG, 95, out)
        bmp.recycle()
        return withOrientation(ctx, out.toByteArray(), ShotSaver.exifOrientation(residual))
    }

    /** Writes the EXIF orientation tag (ExifInterface edits files only, hence the temp file). */
    fun withOrientation(ctx: Context, jpeg: ByteArray, exifOrientation: Int): ByteArray {
        val tmp = File.createTempFile("orient", ".jpg", ctx.cacheDir)
        return try {
            tmp.writeBytes(jpeg)
            ExifInterface(tmp.absolutePath).apply {
                setAttribute(ExifInterface.TAG_ORIENTATION, exifOrientation.toString())
                saveAttributes()
            }
            tmp.readBytes()
        } finally {
            tmp.delete()
        }
    }
}
