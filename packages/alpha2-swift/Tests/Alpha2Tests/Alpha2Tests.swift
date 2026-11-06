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
}
