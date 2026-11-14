package sh.alpha2.consumer

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

/** The library in an app shrunk and obfuscated by R8 with the rules its AAR ships. */
@RunWith(AndroidJUnit4::class)
class ShrunkAppTest {
    private val instrumentation = InstrumentationRegistry.getInstrumentation()
    private val app = instrumentation.targetContext

    private fun asset(name: String): String {
        val file = File(app.cacheDir, name)
        instrumentation.context.assets.open(name).use { input ->
            file.outputStream().use { output -> input.copyTo(output) }
        }
        return file.absolutePath
    }

    @Test
    fun capabilitiesAndAnExportWork() {
        val media = JSONObject(Probe.capabilities(app)).getJSONObject("media")
        assertEquals("android", media.getString("platform"))
        val project = instrumentation.context.assets.open("host-audio-export-project.json")
            .bufferedReader().use { it.readText() }
        val output = File(app.cacheDir, "shrunk-export.mp4")
        val report = JSONObject(
            Probe.export(
                app,
                project,
                mapOf(
                    "image-source" to asset("cross_topology_parity.png"),
                    "audio-source" to asset("audio-only-aac-priming.m4a"),
                ),
                output,
            ),
        )
        assertEquals("aac", report.getString("audioCodec"))
        assertTrue(output.length() > 0)
    }
}
