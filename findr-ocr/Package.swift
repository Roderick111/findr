// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "findr-ocr",
    platforms: [.macOS(.v13)],
    targets: [
        .executableTarget(name: "findr-ocr", path: "Sources")
    ]
)
