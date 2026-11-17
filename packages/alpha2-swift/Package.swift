// swift-tools-version:5.9
// The alpha2 Swift package: the Rust library as a static XCFramework (device and simulator) and
// its uniffi Swift bindings. `scripts/build-ios.sh` builds both into this directory.
import PackageDescription

let package = Package(
    name: "Alpha2",
    platforms: [.iOS(.v17)],
    products: [.library(name: "Alpha2", targets: ["Alpha2"])],
    targets: [
        .binaryTarget(name: "Alpha2FFI", path: "Alpha2FFI.xcframework"),
        .target(
            name: "Alpha2",
            dependencies: ["Alpha2FFI"],
            linkerSettings: [
                .linkedFramework("AudioToolbox"),
                .linkedFramework("AVFAudio"),
                .linkedFramework("CoreAudio"),
                .linkedFramework("CoreFoundation"),
                .linkedFramework("CoreGraphics"),
                .linkedFramework("CoreMedia"),
                .linkedFramework("CoreVideo"),
                .linkedFramework("Foundation"),
                .linkedFramework("IOSurface"),
                .linkedFramework("Metal"),
                .linkedFramework("QuartzCore"),
                .linkedFramework("VideoToolbox"),
                .linkedLibrary("iconv"),
            ]
        ),
        .testTarget(name: "Alpha2Tests", dependencies: ["Alpha2"], resources: [.copy("Fixtures")]),
    ]
)
