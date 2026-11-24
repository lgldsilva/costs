import Foundation

/// Chaves de API consolidadas (`~/.config/agentic-api-keys.env`, o mesmo arquivo do
/// `costs-env.sh`): o app GUI não herda o ambiente do shell e os coletores de quota
/// dependem dessas variáveis. Aplicado com `setenv` antes do primeiro spawn — os
/// `costs` filhos herdam o ambiente do processo.
enum ConsolidatedEnv {
    static let defaultFileURL = URL(fileURLWithPath: NSHomeDirectory())
        .appending(path: ".config/agentic-api-keys.env")

    /// Interpreta linhas `export KEY=value` (sem espaço ao redor do `=`, como o bash
    /// exige; aspas simples/duplas opcionais; `#` é comentário) e aplica cada par.
    /// Arquivo ausente/ilegível é caso normal (0).
    @discardableResult
    static func load(
        fileURL: URL = defaultFileURL,
        apply: (String, String) -> Void = { name, value in setenv(name, value, 1) }
    ) -> Int {
        guard let contents = try? String(contentsOf: fileURL, encoding: .utf8) else { return 0 }
        var applied = 0
        for rawLine in contents.split(separator: "\n", omittingEmptySubsequences: true) {
            guard let (name, value) = parse(String(rawLine)) else { continue }
            apply(name, value)
            applied += 1
        }
        return applied
    }

    private static func parse(_ rawLine: String) -> (name: String, value: String)? {
        let line = rawLine.trimmingCharacters(in: .whitespaces)
        guard !line.isEmpty, !line.hasPrefix("#"), line.hasPrefix("export ") else { return nil }
        let assignment = line.dropFirst("export ".count).trimmingCharacters(in: .whitespaces)
        guard let separator = assignment.firstIndex(of: "=") else { return nil }
        let name = String(assignment[..<separator])
        guard isEnvName(name) else { return nil }
        let raw = String(assignment[assignment.index(after: separator)...]).trimmingCharacters(in: .whitespaces)
        return (name, stripQuotes(raw))
    }

    private static func isEnvName(_ name: String) -> Bool {
        guard let first = name.first, first.isLetter || first == "_" else { return false }
        return name.dropFirst().allSatisfy { $0.isLetter || $0.isNumber || $0 == "_" }
    }

    private static func stripQuotes(_ value: String) -> String {
        guard value.count >= 2, value.first == value.last, value.first == "\"" || value.first == "'" else {
            return value
        }
        return String(value.dropFirst().dropLast())
    }
}
