import Foundation
import Testing
@testable import CostsPreview

@Suite("Chaves de API consolidadas no ambiente do app")
struct ConsolidatedEnvTests {
    private func fixture(_ contents: String) throws -> URL {
        let url = FileManager.default.temporaryDirectory
            .appending(path: "consolidated-env-\(UUID().uuidString).env")
        try contents.write(to: url, atomically: true, encoding: .utf8)
        return url
    }

    @Test func parsesExportLinesAndStripsQuotes() throws {
        var collected: [String: String] = [:]
        let applied = try ConsolidatedEnv.load(fileURL: fixture(#"""
# comentário
export DEEPSEEK_API_KEY=sk-123
export ZAI_API_KEY="com aspas"
export OPENROUTER_API_KEY='aspas simples'
PLAIN=nao-export
export QUEBRADA
export 12BAD=valor
export COM_ESPACO = valor

"""#)) { name, value in
            collected[name] = value
        }
        #expect(applied == 3)
        #expect(collected["DEEPSEEK_API_KEY"] == "sk-123")
        #expect(collected["ZAI_API_KEY"] == "com aspas")
        #expect(collected["OPENROUTER_API_KEY"] == "aspas simples")
    }

    @Test func missingFileIsNormalAndReturnsZero() {
        let missing = FileManager.default.temporaryDirectory
            .appending(path: "nao-existe-\(UUID().uuidString).env")
        #expect(ConsolidatedEnv.load(fileURL: missing) { _, _ in } == 0)
    }
}
