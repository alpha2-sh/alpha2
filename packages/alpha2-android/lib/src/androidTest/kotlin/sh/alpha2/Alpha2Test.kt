package sh.alpha2

import android.graphics.PixelFormat
import android.hardware.HardwareBuffer
import android.media.ImageReader
import android.os.Handler
import android.os.HandlerThread
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import java.io.File
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeFalse
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.json.JSONObject

/** The library through its Kotlin API in an app process, where audio output and windows exist. */
@RunWith(AndroidJUnit4::class)
class Alpha2Test {
    private val instrumentation = InstrumentationRegistry.getInstrumentation()

    @Before
    fun init() {
        Alpha2Android.init(instrumentation.targetContext)
    }

    /** A test asset copied to the app's cache, as an app would hand over a file it owns. */
    private fun asset(name: String): String {
        val file = File(instrumentation.targetContext.cacheDir, name)
        instrumentation.context.assets.open(name).use { input ->
            file.outputStream().use { output -> input.copyTo(output) }
        }
        return file.absolutePath
    }

    private fun text(name: String) =
        instrumentation.context.assets.open(name).bufferedReader().use { it.readText() }

    private fun engine() = Alpha2(EngineConfig(RendererMode.AUTO, VideoDecodeMode.AUTO, VideoFrameMode.AUTO))

    private val audioAssets
        get() = mapOf(
            "image-source" to asset("cross_topology_parity.png"),
            "audio-source" to asset("audio-only-aac-priming.m4a"),
        )

    @Test
    fun capabilitiesNameTheAndroidMediaBackend() {
        engine().use { engine ->
            val media = JSONObject(engine.capabilities()).getJSONObject("media")
            assertEquals("android", media.getString("platform"))
        }
    }

    @Test
    fun aProjectWithAudioExportsToMp4(): Unit = runBlocking {
        engine().use { engine ->
            val output = File(instrumentation.targetContext.cacheDir, "audio-export.mp4")
            val request = ExportRequest(
                projectJson = text("host-audio-export-project.json"),
                assets = audioAssets,
                frames = emptyMap(),
                resources = emptyMap(),
                outputPath = output.absolutePath,
                overwrite = true,
                container = null,
                videoCodec = null,
                audioCodec = null,
                includeAudio = true,
                startTicks = null,
                endTicks = null,
            )
            val progress = object : ExportProgress {
                override fun onProgress(event: ExportProgressEvent) {}
            }
            val outcome = engine.export(request, progress).use { it.outcome() }
            val report = JSONObject(outcome.reportJson)
            assertEquals("aac", report.getString("audioCodec"))
            assertTrue(output.length() > 0)
        }
    }

    @Test
    fun aSurfaceAttachedTwiceInARowKeepsItsWindow() {
        val reader = ImageReader.newInstance(
            64,
            48,
            PixelFormat.RGBA_8888,
            4,
            HardwareBuffer.USAGE_GPU_COLOR_OUTPUT or HardwareBuffer.USAGE_CPU_READ_OFTEN,
        )
        engine().use { engine ->
            val request = PlayerRequest(
                projectJson = text("host-audio-export-project.json"),
                assets = audioAssets,
                frames = emptyMap(),
                resources = emptyMap(),
            )
            Player(engine, request).use { player ->
                Alpha2Android.attach(player, reader.surface, 64u, 48u)
                Alpha2Android.attach(player, reader.surface, 64u, 48u)
            }
            // Each attach took a reference of its own and the closed player released both, so
            // the surface's window is still there to attach.
            Player(engine, request).use { player ->
                Alpha2Android.attach(player, reader.surface, 64u, 48u)
            }
        }
        reader.close()
    }

    @Test
    fun aThrowingProgressCallbackFailsTheExportAndLeavesNoFile(): Unit = runBlocking {
        engine().use { engine ->
            val output = File(instrumentation.targetContext.cacheDir, "throwing-progress.mp4")
            output.delete()
            val request = ExportRequest(
                projectJson = text("host-audio-export-project.json"),
                assets = audioAssets,
                frames = emptyMap(),
                resources = emptyMap(),
                outputPath = output.absolutePath,
                overwrite = true,
                container = null,
                videoCodec = null,
                audioCodec = null,
                includeAudio = false,
                startTicks = null,
                endTicks = null,
            )
            val progress = object : ExportProgress {
                override fun onProgress(event: ExportProgressEvent) {
                    throw IllegalStateException("the app broke")
                }
            }
            val failure = withTimeout(60_000) {
                engine.export(request, progress).use { runCatching { it.outcome() }.exceptionOrNull() }
            }
            assertTrue("$failure", failure is Alpha2Exception.Failed)
            val failed = failure as Alpha2Exception.Failed
            assertEquals("PROGRESS_CALLBACK_FAILED", failed.code)
            assertTrue(failed.reason, failed.reason.contains("the app broke"))
            assertFalse(output.exists())
        }
    }

    @Test
    fun aPreviewPlaysWithSoundIntoASurface() {
        val drawing = HandlerThread("alpha2-test-compositor").apply { start() }
        // An image reader's surface stands in for a view's; its images are taken as they come,
        // as a compositor would.
        val reader = ImageReader.newInstance(
            64,
            48,
            PixelFormat.RGBA_8888,
            4,
            HardwareBuffer.USAGE_GPU_COLOR_OUTPUT or HardwareBuffer.USAGE_CPU_READ_OFTEN,
        )
        reader.setOnImageAvailableListener({ it.acquireLatestImage()?.close() }, Handler(drawing.looper))
        engine().use { engine ->
            val request = PlayerRequest(
                projectJson = text("host-audio-export-project.json"),
                assets = audioAssets,
                frames = emptyMap(),
                resources = emptyMap(),
            )
            Player(engine, request).use { player ->
                Alpha2Android.attach(player, reader.surface, 64u, 48u)
                // A GPU that refuses the window (GL-only emulator images) skips, as the Rust
                // preview tests do; any other failure fails.
                val until = System.currentTimeMillis() + 10_000
                while (player.framesPresented() == 0uL && player.lastError() == null &&
                    System.currentTimeMillis() < until) {
                    Thread.sleep(5)
                }
                val refused = player.lastError()
                assumeFalse(
                    "skipped: $refused",
                    refused?.startsWith("the GPU cannot present to this window") == true,
                )
                assertNull(refused)
                val (presented, dropped) = player.framesPresented() to player.framesDropped()
                player.play()
                var sounding = false
                val playing = System.currentTimeMillis() + 10_000
                while (System.currentTimeMillis() < playing) {
                    sounding = sounding || player.isSounding()
                    if (sounding && !player.isPlaying()) break
                    Thread.sleep(5)
                }
                assertTrue("the project's audio played", sounding)
                assertTrue(!player.isPlaying())
                // Each of the project's 12 frames is drawn or, on a device too slow for the
                // rate, counted as dropped.
                val drawn = player.framesPresented() - presented
                val skipped = player.framesDropped() - dropped
                assertEquals("$drawn drawn, $skipped dropped", 12uL, drawn + skipped)
                assertNull(player.lastError())
                player.detachWindow()
            }
        }
        reader.close()
        drawing.quitSafely()
    }

    @Test
    fun aPausedSessionSilencesItsPreviewAndPlaysOnWhenResumed() {
        val drawing = HandlerThread("alpha2-test-compositor").apply { start() }
        val reader = ImageReader.newInstance(
            64,
            48,
            PixelFormat.RGBA_8888,
            4,
            HardwareBuffer.USAGE_GPU_COLOR_OUTPUT or HardwareBuffer.USAGE_CPU_READ_OFTEN,
        )
        reader.setOnImageAvailableListener({ it.acquireLatestImage()?.close() }, Handler(drawing.looper))
        // Five seconds, so the preview is still playing when the session pauses.
        val project = JSONObject(text("host-audio-export-project.json"))
        val tracks = project.getJSONObject("timeline").getJSONArray("tracks")
        for (track in 0 until tracks.length()) {
            val clips = tracks.getJSONObject(track).getJSONArray("clips")
            for (clip in 0 until clips.length()) {
                clips.getJSONObject(clip).getJSONObject("timeline_range").put("end", 600_000)
                clips.getJSONObject(clip).getJSONObject("source_range").put("end", 600_000)
            }
        }
        fun waitFor(millis: Long, done: () -> Boolean): Boolean {
            val until = System.currentTimeMillis() + millis
            while (!done() && System.currentTimeMillis() < until) Thread.sleep(5)
            return done()
        }
        engine().use { engine ->
            val request = PlayerRequest(
                projectJson = project.toString(),
                assets = audioAssets,
                frames = emptyMap(),
                resources = emptyMap(),
            )
            Player(engine, request).use { player ->
                Alpha2Android.attach(player, reader.surface, 64u, 48u)
                waitFor(10_000) { player.framesPresented() > 0uL || player.lastError() != null }
                val refused = player.lastError()
                assumeFalse(
                    "skipped: $refused",
                    refused?.startsWith("the GPU cannot present to this window") == true,
                )
                assertNull(refused)
                player.play()
                assertTrue("sounding", waitFor(5_000) { player.isSounding() })
                engine.pause()
                assertTrue("silent while paused", waitFor(2_000) { !player.isSounding() })
                assertTrue(player.isPlaying())
                val held = player.positionTicks()
                Thread.sleep(300)
                assertEquals(held, player.positionTicks())
                engine.resume()
                assertTrue("sounding again", waitFor(5_000) { player.isSounding() })
                assertTrue("moving again", waitFor(5_000) { player.positionTicks() > held })
                assertNull(player.lastError())
                player.pause()
                player.detachWindow()
            }
        }
        reader.close()
        drawing.quitSafely()
    }

    @Test
    fun aDiagnosticListenerReceivesThePreviewsLines() {
        val lines = LinkedBlockingQueue<String>()
        setDiagnosticListener(
            object : DiagnosticListener {
                override fun onDiagnostic(line: String) {
                    lines.add(line)
                }
            },
        )
        try {
            engine().use { engine ->
                // The audio source is a picture, so the preview's audio decode fails and says so.
                val picture = asset("cross_topology_parity.png")
                val request = PlayerRequest(
                    projectJson = text("host-audio-export-project.json"),
                    assets = mapOf("image-source" to picture, "audio-source" to picture),
                    frames = emptyMap(),
                    resources = emptyMap(),
                )
                Player(engine, request).use { player ->
                    player.play()
                    val deadline = System.currentTimeMillis() + 20_000
                    var found = false
                    while (!found && System.currentTimeMillis() < deadline) {
                        val line = lines.poll(100, TimeUnit.MILLISECONDS) ?: continue
                        found = line.contains("alpha2 preview: the project's audio does not decode")
                    }
                    assertTrue("lines: $lines", found)
                }
            }
        } finally {
            setDiagnosticListener(null)
        }
    }
}
