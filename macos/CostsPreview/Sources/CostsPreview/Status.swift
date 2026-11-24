import Foundation
import SwiftUI

struct StatusSnapshot: Decodable {
    let month: String?
    let spendUSD: Double?
    let subscriptionsUSDMonth: Double?
    let syncAge: String?
    let stale: Bool?
    let useOrder: [String]?
    let providers: [Provider]

    enum CodingKeys: String, CodingKey {
        case month, stale, providers
        case spendUSD = "spend_usd"
        case subscriptionsUSDMonth = "subscriptions_usd_month"
        case syncAge = "sync_age"
        case useOrder = "use_order"
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        month = try values.decodeIfPresent(String.self, forKey: .month)
        spendUSD = try values.decodeIfPresent(Double.self, forKey: .spendUSD)
        subscriptionsUSDMonth = try values.decodeIfPresent(Double.self, forKey: .subscriptionsUSDMonth)
        syncAge = try values.decodeIfPresent(String.self, forKey: .syncAge)
        stale = try values.decodeIfPresent(Bool.self, forKey: .stale)
        useOrder = try values.decodeIfPresent([String].self, forKey: .useOrder)
        providers = try values.decodeIfPresent([Provider].self, forKey: .providers) ?? []
    }
}

struct Provider: Decodable, Identifiable {
    let id: String
    let windows: [QuotaWindow]
    let agents: [AgentSpend]
    let costUSD: Double?
    let subscriptionUSDMonth: Double?
    let plan: String?
    let balance: String?
    let errorText: String?
    let errorHint: String?
    let errorIcon: String?
    let ok: Bool?
    let stale: Bool?
    let updated: String?
    let exhaustedReset: String?
    let resets: ResetCredits?
    let resetsWeek: ResetCredits?
    let priorityHint: String?

    enum CodingKeys: String, CodingKey {
        case id, windows, agents, balance, ok, stale, updated, resets
        case resetsWeek = "resets_week"
        case costUSD = "cost_usd"
        case subscriptionUSDMonth = "subscription_usd_month"
        case plan
        case errorText = "error_text"
        case errorHint = "error_hint"
        case errorIcon = "error_icon"
        case exhaustedReset = "exhausted_reset"
        case priorityHint = "priority_hint"
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        id = try values.decode(String.self, forKey: .id)
        windows = try values.decodeIfPresent([QuotaWindow].self, forKey: .windows) ?? []
        agents = try values.decodeIfPresent([AgentSpend].self, forKey: .agents) ?? []
        costUSD = try values.decodeIfPresent(Double.self, forKey: .costUSD)
        subscriptionUSDMonth = try values.decodeIfPresent(Double.self, forKey: .subscriptionUSDMonth)
        plan = try values.decodeIfPresent(String.self, forKey: .plan)
        balance = try values.decodeIfPresent(String.self, forKey: .balance)
        errorText = try values.decodeIfPresent(String.self, forKey: .errorText)
        errorHint = try values.decodeIfPresent(String.self, forKey: .errorHint)
        errorIcon = try values.decodeIfPresent(String.self, forKey: .errorIcon)
        ok = try values.decodeIfPresent(Bool.self, forKey: .ok)
        stale = try values.decodeIfPresent(Bool.self, forKey: .stale)
        updated = try values.decodeIfPresent(String.self, forKey: .updated)
        exhaustedReset = try values.decodeIfPresent(String.self, forKey: .exhaustedReset)
        resets = try values.decodeIfPresent(ResetCredits.self, forKey: .resets)
        resetsWeek = try values.decodeIfPresent(ResetCredits.self, forKey: .resetsWeek)
        priorityHint = try values.decodeIfPresent(String.self, forKey: .priorityHint)
    }
}

struct QuotaWindow: Decodable, Identifiable {
    let label: String
    let kind: WindowKind?
    let usedPercent: Double?
    let color: String?
    let reset: String?
    let resetsAt: Double?
    let sparePercent: Double?
    let text: String?
    let hint: String?
    let wait: String?

    var id: String { label }

    enum CodingKeys: String, CodingKey {
        case label, kind, color, reset, text, hint, wait
        case usedPercent = "used_pct"
        case resetsAt = "resets_at"
        case sparePercent = "spare_pct"
    }
}

enum WindowKind: String, Decodable, CaseIterable {
    case session, week, month, other

    static func legacyKind(for label: String) -> WindowKind? {
        switch label.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() {
        case "5h", "session", "sessão", "sessao": .session
        case "sem", "week", "weekly", "semanal", "7d", "mcp": .week
        case "mês", "mes", "month", "monthly", "30d": .month
        default: nil
        }
    }

    var ringOrder: Int {
        switch self {
        case .month: 0
        case .week: 1
        case .session: 2
        case .other: 3
        }
    }
}

struct GaugeRing: Equatable {
    let label: String
    let usedPercent: Double
    let color: String?
    let kind: WindowKind

    static func select(from windows: [QuotaWindow]) -> [GaugeRing] {
        var selected: [(window: QuotaWindow, kind: WindowKind)] = []
        for window in windows {
            guard let used = window.usedPercent, used.isFinite else { continue }
            guard let kind = window.kind ?? WindowKind.legacyKind(for: window.label) else { continue }
            selected.append((window, kind))
        }

        var distinct: [WindowKind: QuotaWindow] = [:]
        for entry in selected where distinct[entry.kind] == nil {
            distinct[entry.kind] = entry.window
        }
        // Prefer explicit backend kinds to legacy-label classification.
        for entry in selected where entry.window.kind != nil {
            distinct[entry.kind] = entry.window
        }
        return distinct.keys.sorted { $0.ringOrder < $1.ringOrder }.compactMap { kind in
            guard let window = distinct[kind], let used = window.usedPercent else { return nil }
            return GaugeRing(label: window.label, usedPercent: used, color: window.color, kind: kind)
        }
    }
}

struct AgentSpend: Decodable, Identifiable {
    let label: String
    let usd: Double?
    let tokens: Int64?
    var id: String { label }
}

struct ExpiryGroup: Decodable, Identifiable {
    let available: Int
    let expiresAtRaw: String
    var id: String { expiresAtRaw }

    enum CodingKeys: String, CodingKey {
        case available
        case expiresAtRaw = "expires_at_raw"
    }
}

struct ResetCredits: Decodable {
    let available: Int?
    let nextExpiry: String?
    let expiryGroups: [ExpiryGroup]?

    enum CodingKeys: String, CodingKey {
        case available
        case nextExpiry = "next_expiry"
        case expiryGroups = "expiry_groups"
    }
}

enum ProviderCatalog {
    struct Entry {
        let id: String
        let name: String
        let monogram: String
        let color: RGBColor
    }

    struct RGBColor {
        let red: Double
        let green: Double
        let blue: Double
    }

    static let names: [Entry] = [
        Entry(id: "claude", name: "Claude", monogram: "C", color: RGBColor(red: 0.85, green: 0.47, blue: 0.34)),
        Entry(id: "codex", name: "Codex", monogram: "CX", color: RGBColor(red: 0.06, green: 0.64, blue: 0.50)),
        Entry(id: "zai", name: "Z.ai", monogram: "Z", color: RGBColor(red: 0.23, green: 0.36, blue: 0.90)),
        Entry(id: "xai", name: "Grok", monogram: "X", color: RGBColor(red: 0.15, green: 0.15, blue: 0.17)),
        Entry(id: "opencode-go", name: "OpenCode Go", monogram: "OC", color: RGBColor(red: 0.43, green: 0.34, blue: 0.81)),
        Entry(id: "deepseek", name: "DeepSeek", monogram: "DS", color: RGBColor(red: 0.30, green: 0.42, blue: 1.00)),
        Entry(id: "openrouter", name: "OpenRouter", monogram: "OR", color: RGBColor(red: 0.40, green: 0.45, blue: 0.55)),
        Entry(id: "antigravity", name: "Antigravity", monogram: "AG", color: RGBColor(red: 0.48, green: 0.38, blue: 1.00)),
    ]
    static func name(for id: String) -> String { names.first { $0.id == id }?.name ?? id }
    static func monogram(for id: String) -> String { names.first { $0.id == id }?.monogram ?? "?" }
    static func color(for id: String) -> Color {
        let rgb = names.first { $0.id == id }?.color ?? RGBColor(red: 0.5, green: 0.5, blue: 0.5)
        return Color(red: rgb.red, green: rgb.green, blue: rgb.blue)
    }
}

/// Selo com as iniciais do provedor: legível em qualquer tamanho, sem depender de símbolos.
struct ProviderBadge: View {
    let id: String
    var size: CGFloat = 26
    var fontSize: CGFloat = 11
    var body: some View {
        ZStack {
            RoundedRectangle(cornerRadius: size * 0.32).fill(ProviderCatalog.color(for: id))
            Text(ProviderCatalog.monogram(for: id))
                .font(.system(size: fontSize, weight: .bold, design: .rounded))
                .foregroundStyle(.white)
                .minimumScaleFactor(0.6)
        }.frame(width: size, height: size)
    }
}

enum DisplayFormat {
    static func usd(_ value: Double?) -> String {
        guard let value, value.isFinite else { return "Indisponível" }
        return value.formatted(.currency(code: "USD"))
    }
    static func percentage(_ value: Double?) -> String {
        guard let value, value.isFinite else { return "Indisponível" }
        return "\(Int(value.rounded()))%"
    }
    static func tokens(_ value: Int64?) -> String {
        guard let value, value >= 0 else { return "Indisponível" }
        return value.formatted(.number.notation(.compactName))
    }
    static func resetCountdown(until timestamp: Double?, now: Date = .now) -> String? {
        guard let timestamp, timestamp.isFinite else { return nil }
        let seconds = timestamp - now.timeIntervalSince1970
        guard seconds > 0 else { return "em breve" }
        let hours = Int(seconds / 3_600)
        if hours >= 24 { return "em \(hours / 24)d \(hours % 24)h" }
        if hours > 0 { return "em \(hours)h \(Int(seconds.truncatingRemainder(dividingBy: 3_600) / 60))m" }
        return "em \(max(1, Int(seconds / 60)))m"
    }
}
