import Foundation
import XCTest

@testable import Alpha2

/// The library through its Swift API, on the renderer the session picks.
final class Alpha2Tests: XCTestCase {
    private func fixture(_ name: String) throws -> URL {
        try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: nil, subdirectory: "Fixtures"))
    }

    private func engine() -> Alpha2 {
        Alpha2(config: EngineConfig(renderer: .auto, videoDecode: .auto, videoFrames: .auto))
    }

    func testCapabilitiesNameTheApplePlatform() throws {
        let report = try JSONSerialization.jsonObject(with: Data(engine().capabilities().utf8)) as? [String: Any]
        let media = try XCTUnwrap(report?["media"] as? [String: Any])
        XCTAssertEqual(media["platform"] as? String, "ios")
    }

    func testAProjectThatDoesNotLoadIsRefusedWithItsProblems() {
        XCTAssertThrowsError(try engine().stills(request: StillsRequest(
            projectJson: "{}", assets: [:], frames: [:], resources: [:],
            stills: [StillEntry(timeTicks: 0, outputPath: NSTemporaryDirectory() + "x.png")],
            alpha: false, overwrite: true
        ))) { error in
            guard case Alpha2Error.InvalidInput = error else { return XCTFail("\(error)") }
        }
    }

    private final class ThrowingProgress: ExportProgress {
        func onProgress(event: ExportProgressEvent) throws {
            throw ProgressFailure.Failed(reason: "the app broke")
        }
    }

    func testAThrowingProgressCallbackFailsTheExportAndLeavesNoFile() async throws {
        let project = try String(contentsOf: fixture("host-parity-project.json"), encoding: .utf8)
        let output = NSTemporaryDirectory() + "alpha2-throwing-progress.mp4"
        try? FileManager.default.removeItem(atPath: output)
        let handle = try engine().export(request: ExportRequest(
            projectJson: project,
            assets: ["image-source": try fixture("cross_topology_parity.png").path],
            frames: [:], resources: [:], outputPath: output, overwrite: true,
            container: nil, videoCodec: nil, audioCodec: nil, includeAudio: false,
            startTicks: nil, endTicks: nil
        ), progress: ThrowingProgress())
        let settled = expectation(description: "the outcome settles")
        Task {
            do {
                _ = try await handle.outcome()
                XCTFail("the export finished")
            } catch Alpha2Error.Failed(let code, let reason, _) {
                XCTAssertEqual(code, "PROGRESS_CALLBACK_FAILED")
                XCTAssertTrue(reason.contains("the app broke"), reason)
            } catch {
                XCTFail("\(error)")
            }
            settled.fulfill()
        }
        await fulfillment(of: [settled], timeout: 60)
        XCTAssertFalse(FileManager.default.fileExists(atPath: output))
    }

    func testStillsArePngFiles() throws {
        let project = try String(contentsOf: fixture("host-parity-project.json"), encoding: .utf8)
        let output = NSTemporaryDirectory() + "alpha2-still.png"
        let result = try engine().stills(request: StillsRequest(
            projectJson: project,
            assets: ["image-source": try fixture("cross_topology_parity.png").path],
            frames: [:], resources: [:],
            stills: [StillEntry(timeTicks: 0, outputPath: output)],
            alpha: false, overwrite: true
        ))
        XCTAssertTrue(result.contains("\"width\":64"), result)
        XCTAssertTrue(FileManager.default.fileExists(atPath: output))
    }

    private final class Lines: DiagnosticListener {
        private let lock = NSLock()
        private var lines: [String] = []

        func onDiagnostic(line: String) throws {
            lock.lock()
            lines.append(line)
            lock.unlock()
        }

        func contains(_ text: String) -> Bool {
            lock.lock()
            defer { lock.unlock() }
            return lines.contains { $0.contains(text) }
        }
    }

    func testADiagnosticListenerReceivesThePreviewsLines() throws {
        let lines = Lines()
        setDiagnosticListener(listener: lines)
        defer { setDiagnosticListener(listener: nil) }
        // The audio source has no file, so the preview opens no audio and says so.
        let player = try Player(engine: engine(), request: PlayerRequest(
            projectJson: try String(contentsOf: fixture("host-audio-export-project.json"), encoding: .utf8),
            assets: ["image-source": try fixture("cross_topology_parity.png").path],
            frames: [:], resources: [:]
        ))
        player.play()
        let expected = "alpha2 preview: no audio: audio source audio-source has no asset path"
        let deadline = Date().addingTimeInterval(20)
        while !lines.contains(expected), Date() < deadline {
            Thread.sleep(forTimeInterval: 0.05)
        }
        player.end()
        XCTAssertTrue(lines.contains(expected))
    }
}
