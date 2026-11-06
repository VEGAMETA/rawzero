package dev.vegameta.rawzero.ui

import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.RectF
import android.view.View

/** Arc drawn around the shutter while a long exposure runs; [progress] in 0..1. */
class ProgressRing(context: Context) : View(context) {
    private val stroke = 5 * resources.displayMetrics.density
    private val track = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE; strokeWidth = stroke; color = Color.argb(70, 255, 255, 255)
    }
    private val arc = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        style = Paint.Style.STROKE; strokeWidth = stroke; strokeCap = Paint.Cap.ROUND; color = Color.rgb(255, 204, 0)
    }
    private val box = RectF()

    var progress = 0f
        set(v) {
            field = v.coerceIn(0f, 1f)
            invalidate()
        }

    override fun onDraw(c: Canvas) {
        val h = stroke / 2
        box.set(h, h, width - h, height - h)
        c.drawOval(box, track)
        c.drawArc(box, -90f, 360f * progress, false, arc)
    }
}
