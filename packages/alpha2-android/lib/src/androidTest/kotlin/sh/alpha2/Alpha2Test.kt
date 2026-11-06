package sh.alpha2

import android.graphics.PixelFormat
import android.hardware.HardwareBuffer
import android.media.ImageReader
import android.os.Handler
import android.os.HandlerThread
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import java.io.File
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
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
                val window = Alpha2Android.windowFromSurface(reader.surface)
                assertTrue(window != 0L)
                player.attachWindow(window.toULong(), 64u, 48u)
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
}
