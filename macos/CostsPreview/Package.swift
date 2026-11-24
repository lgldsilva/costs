// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "CostsPreview",
    platforms: [.macOS(.v14)],
    products: [.executable(name: "CostsPreview", targets: ["CostsPreview"])],
    targets: [
        .executableTarget(
            name: "CostsPreview",
            sources: [
                "AppModel.swift", "ConsolidatedEnv.swift", "CostsPreviewApp.swift", "LoginService.swift",
                "StartupSupport.swift", "Status.swift", "StatusLoader.swift", "Views.swift", "main.swift",
            ]
        ),
        .testTarget(name: "CostsPreviewTests", dependencies: ["CostsPreview"]),
    ]
)
