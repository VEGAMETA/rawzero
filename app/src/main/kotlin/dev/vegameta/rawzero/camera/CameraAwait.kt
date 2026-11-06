package dev.vegameta.rawzero.camera

import android.hardware.camera2.CameraAccessException
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CaptureFailure
import android.hardware.camera2.CaptureRequest
import android.hardware.camera2.TotalCaptureResult
import android.hardware.camera2.params.OutputConfiguration
import android.hardware.camera2.params.SessionConfiguration
import android.os.Handler
import kotlinx.coroutines.suspendCancellableCoroutine
import java.util.concurrent.Executor
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException

/** Coroutine wrappers over the Camera2 callback APIs. Late callbacks after cancellation close what they deliver. */

/**
 * Opens a camera. [onLost] fires if the device fails or disconnects *after* it was opened
 * (e.g. the Samsung HAL erroring out after a long frame); the caller should reopen.
 */
suspend fun CameraManager.openAwait(id: String, handler: Handler, onLost: (String) -> Unit = {}): CameraDevice =
    suspendCancellableCoroutine { cont ->
        try {
            openCamera(id, object : CameraDevice.StateCallback() {
                override fun onOpened(d: CameraDevice) {
                    if (cont.isActive) cont.resume(d) else d.close()
                }
                override fun onDisconnected(d: CameraDevice) {
                    d.close()
                    if (cont.isActive) cont.resumeWithException(
                        CameraAccessException(CameraAccessException.CAMERA_DISCONNECTED, "camera $id disconnected"))
                    else onLost("disconnected")
                }
                override fun onError(d: CameraDevice, error: Int) {
                    d.close()
                    if (cont.isActive) cont.resumeWithException(IllegalStateException("camera $id open error $error"))
                    else onLost("error $error")
                }
            }, handler)
        } catch (e: Exception) {
            if (cont.isActive) cont.resumeWithException(e)
        }
    }

suspend fun CameraDevice.createSessionAwait(outputs: List<OutputConfiguration>, exec: Executor): CameraCaptureSession =
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
            createCaptureSession(cfg)
        } catch (e: Exception) {
            if (cont.isActive) cont.resumeWithException(e)
        }
    }

suspend fun CameraCaptureSession.captureAwait(request: CaptureRequest, handler: Handler): TotalCaptureResult =
    suspendCancellableCoroutine { cont ->
        capture(request, object : CameraCaptureSession.CaptureCallback() {
            override fun onCaptureCompleted(s: CameraCaptureSession, r: CaptureRequest, res: TotalCaptureResult) {
                if (cont.isActive) cont.resume(res)
            }
            override fun onCaptureFailed(s: CameraCaptureSession, r: CaptureRequest, f: CaptureFailure) {
                if (cont.isActive) cont.resumeWithException(IllegalStateException("capture failed, reason ${f.reason}"))
            }
        }, handler)
    }
