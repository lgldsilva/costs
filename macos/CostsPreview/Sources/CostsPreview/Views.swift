import SwiftUI

struct PreviewPanel: View {
    @Environment(PreviewModel.self) private var model
    @State private var expandedProviders: Set<String> = []

    var body: some View {
        VStack(spacing: 0) {
            titlebar
            totalsBar
            Divider().overlay(Color.primary.opacity(0.04))
            providerPicker
            Divider().overlay(Color.primary.opacity(0.04))
            ScrollView {
                VStack(alignment: .leading, spacing: 14) {
                    if let error = model.errorMessage { errorBanner(error) }
                    if let provider = model.selectedProvider { providerCard(provider).id(provider.id) }
                    else if model.isLoading { ProgressView("Lendo resumo…").frame(maxWidth: .infinity).padding(32) }
                    else { ContentUnavailableView("Sem provedores", systemImage: "tray", description: Text("O resumo não contém provedores disponíveis.")) }
                    actions
                }.padding(8)
            }.scrollIndicators(.hidden)
            footer
        }
        .frame(width: 364)
        .frame(maxHeight: 940)
        .accessibilityIdentifier("costs-preview-panel")
        .background(.regularMaterial)
        .preferredColorScheme(appearanceScheme)
        .onAppear { model.loadIfNeeded() }
    }

    private var appearanceScheme: ColorScheme? {
        switch ProcessInfo.processInfo.environment["COSTS_PREVIEW_APPEARANCE"] {
        case "light": .light
        case "dark": .dark
        default: nil
        }
    }

    private var titlebar: some View {
        HStack(spacing: 12) {
            ZStack {
                RoundedRectangle(cornerRadius: 12).fill(Color.accentColor.gradient).frame(width: 38, height: 38)
                Image(systemName: "waveform.path.ecg").font(.system(size: 17, weight: .semibold)).foregroundStyle(.white)
            }
            VStack(alignment: .leading, spacing: 2) {
                Text("Costs Prévia").font(.system(size: 16, weight: .semibold, design: .rounded))
                Text("Cotas e custos, em um só lugar").font(.system(size: 11)).foregroundStyle(.secondary)
            }
            Spacer()
            Button { model.refresh(quota: model.isFixture ? .none : .force) } label: {
                Image(systemName: "arrow.clockwise").font(.system(size: 13, weight: .medium)).frame(width: 30, height: 30).background(.quaternary, in: Circle())
            }.buttonStyle(.plain).disabled(model.isLoading || model.isFixture)
                .accessibilityLabel("Atualizar cotas e resumo")
                .accessibilityIdentifier("refresh-status")
                .help(model.isFixture ? "Modo de demonstração: arquivo de snapshot somente leitura" : "Consultar cotas na rede e reler o resumo")
        }.padding(.horizontal, 5).padding(.vertical, 6)
    }

    /// Totais no topo: estimativa API do mês e assinaturas, compactos.
    private var totalsBar: some View {
        HStack(spacing: 0) {
            metric(title: "API · estimativa", value: DisplayFormat.usd(model.snapshot?.spendUSD), detail: model.snapshot?.month ?? "Mês atual")
            Rectangle().fill(Color.primary.opacity(0.08)).frame(width: 1, height: 28).padding(.horizontal, 8)
            metric(title: "Assinaturas", value: DisplayFormat.usd(model.snapshot?.subscriptionsUSDMonth), detail: "por mês")
        }
        .padding(.horizontal, 6).padding(.vertical, 7)
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("costs-preview-totals")
    }

    /// Seletor em grade compacta: quebra em várias linhas, ícones pequenos.
    private var providerPicker: some View {
        let columns = Array(repeating: GridItem(.flexible(), spacing: 6), count: 4)
        return LazyVGrid(columns: columns, spacing: 6) {
            ForEach(model.availableProviders) { provider in
                let chosen = model.selectedID == provider.id
                Button { model.selectedID = provider.id } label: {
                    VStack(spacing: 3) {
                        ProviderBadge(id: provider.id, size: 22, fontSize: 9)
                        Text(ProviderCatalog.name(for: provider.id)).font(.system(size: 9, weight: .medium)).lineLimit(1).minimumScaleFactor(0.8)
                    }
                    .foregroundStyle(chosen ? Color.accentColor : Color.secondary)
                    .frame(maxWidth: .infinity).frame(height: 42)
                    .background(chosen ? Color.accentColor.opacity(0.09) : Color.clear, in: RoundedRectangle(cornerRadius: 9))
                    .overlay(RoundedRectangle(cornerRadius: 9).strokeBorder(chosen ? Color.accentColor.opacity(0.24) : Color.primary.opacity(0.06), lineWidth: 1))
                }.buttonStyle(.plain)
                    .accessibilityLabel(ProviderCatalog.name(for: provider.id))
                    .accessibilityIdentifier("provider-\(provider.id)")
                    .accessibilityAddTraits(chosen ? .isSelected : [])
            }
        }.padding(.horizontal, 6).padding(.vertical, 7)
    }

    private func metric(title: String, value: String, detail: String) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(title.uppercased()).font(.system(size: 8, weight: .semibold, design: .rounded)).tracking(0.6).foregroundStyle(.secondary)
            Text(value).font(.system(size: 18, weight: .semibold, design: .rounded).monospacedDigit()).lineLimit(1).minimumScaleFactor(0.8)
            Text(detail).font(.system(size: 10)).foregroundStyle(.secondary)
        }.frame(maxWidth: .infinity, alignment: .leading)
    }

    private func providerCard(_ provider: Provider) -> some View {
        VStack(alignment: .leading, spacing: 13) {
            HStack(spacing: 10) {
                ProviderBadge(id: provider.id, size: 40, fontSize: 15)
                VStack(alignment: .leading, spacing: 3) {
                    Text(ProviderCatalog.name(for: provider.id)).font(.system(size: 19, weight: .semibold, design: .rounded))
                    Text(provider.ok == false ? (provider.errorText ?? "Não foi possível consultar a cota") : (provider.updated ?? "Atualização indisponível"))
                        .font(.system(size: 10)).foregroundStyle(.secondary)
                }
                Spacer()
                if let rank = model.recommendationRank {
                    VStack(alignment: .trailing, spacing: 3) {
                        Text("USAR AGORA").font(.system(size: 8, weight: .semibold)).tracking(0.5).foregroundStyle(.secondary)
                        Text("#\(rank)").font(.system(size: 14, weight: .semibold, design: .rounded).monospacedDigit()).foregroundStyle(Color.accentColor)
                    }
                }
            }
            if let plan = provider.plan?.trimmingCharacters(in: .whitespacesAndNewlines), !plan.isEmpty {
                detailRow("Plano detectado", value: plan, symbol: "person.text.rectangle")
                    .accessibilityIdentifier("provider-plan-\(provider.id)")
            }
            if provider.stale == true { statusPill("Cota desatualizada", icon: "clock.arrow.circlepath", color: .orange) }
            if provider.ok == false { errorBanner(provider.errorHint ?? provider.errorText ?? "A consulta falhou; os últimos valores permanecem visíveis.") }
            if provider.windows.isEmpty {
                HStack(spacing: 10) {
                    Image(systemName: "minus.circle").foregroundStyle(.secondary)
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Sem janela de cota").font(.system(size: 12, weight: .medium))
                        Text("Este provedor não publicou limites no resumo.").font(.system(size: 10)).foregroundStyle(.secondary)
                    }
                }.padding(11).frame(maxWidth: .infinity, alignment: .leading).background(.quaternary.opacity(0.45), in: RoundedRectangle(cornerRadius: 11))
            } else {
                VStack(spacing: 0) {
                    ForEach(Array(provider.windows.enumerated()), id: \.element.id) { index, window in
                        windowRow(window)
                        if index < provider.windows.count - 1 { Divider().padding(.vertical, 11) }
                    }
                }
            }
            if let balance = provider.balance { detailRow("Saldo", value: balance, symbol: "creditcard") }
            ForEach(Array(SnapshotPresentation.resetRows(provider.resets, fallbackTitle: "Resets de sessão (5h)").enumerated()), id: \.offset) { _, row in
                detailRow(row.title, value: row.value, symbol: "arrow.counterclockwise.circle")
            }
            ForEach(Array(SnapshotPresentation.resetRows(provider.resetsWeek, fallbackTitle: "Resets semanais").enumerated()), id: \.offset) { _, row in
                detailRow(row.title, value: row.value, symbol: "arrow.triangle.2.circlepath.circle")
                    .accessibilityIdentifier("provider-resets-week-\(provider.id)")
            }
            if let reset = provider.exhaustedReset { detailRow("Janela esgotada", value: reset, symbol: "hourglass") }
            if let hint = provider.priorityHint { Text(hint).font(.system(size: 10)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true) }
            HStack(spacing: 0) {
                metric(title: "API · estimativa", value: DisplayFormat.usd(provider.costUSD), detail: "neste mês")
                Rectangle().fill(Color.primary.opacity(0.08)).frame(width: 1, height: 32).padding(.horizontal, 11)
                metric(title: "Assinatura", value: DisplayFormat.usd(provider.subscriptionUSDMonth), detail: "mensal")
            }.padding(.top, 2)
            if !provider.agents.isEmpty {
                DisclosureGroup(isExpanded: expandedBinding(for: provider.id)) {
                    VStack(spacing: 7) {
                        ForEach(provider.agents) { agent in
                            HStack(spacing: 7) {
                                Text(agent.label).font(.system(size: 9)).lineLimit(1).truncationMode(.middle)
                                Spacer(minLength: 4)
                                Text(DisplayFormat.usd(agent.usd)).font(.system(size: 9, design: .monospaced)).foregroundStyle(.secondary)
                                Text("· \(DisplayFormat.tokens(agent.tokens)) tok.").font(.system(size: 8)).foregroundStyle(.tertiary)
                            }
                        }
                    }.padding(.top, 8)
                } label: {
                    Label("Detalhe por máquina e ferramenta", systemImage: "chevron.down.circle").font(.system(size: 10, weight: .medium)).foregroundStyle(.secondary)
                }.tint(.secondary)
            }
        }.padding(8).background(.background.opacity(0.8), in: RoundedRectangle(cornerRadius: 14))
            .overlay(RoundedRectangle(cornerRadius: 17).strokeBorder(Color.primary.opacity(0.07), lineWidth: 1))
    }

    private func expandedBinding(for providerID: String) -> Binding<Bool> {
        Binding(get: { expandedProviders.contains(providerID) }, set: { isExpanded in
            if isExpanded { expandedProviders.insert(providerID) } else { expandedProviders.remove(providerID) }
        })
    }

    private func windowRow(_ window: QuotaWindow) -> some View {
        VStack(alignment: .leading, spacing: 7) {
            HStack(alignment: .firstTextBaseline) {
                Text(window.label.uppercased()).font(.system(size: 10, weight: .semibold, design: .rounded)).tracking(0.5)
                Spacer()
                Text(DisplayFormat.percentage(window.usedPercent)).font(.system(size: 13, weight: .semibold, design: .rounded).monospacedDigit())
                Text("usado").font(.system(size: 9)).foregroundStyle(.secondary)
            }
            if let used = window.usedPercent, used.isFinite {
                ProgressView(value: min(max(used, 0), 100), total: 100).tint(quotaColor(window.color))
                    .scaleEffect(x: 1, y: 0.85, anchor: .center).accessibilityLabel("\(window.label): \(DisplayFormat.percentage(used)) usado")
            } else { ProgressView(value: 0).tint(.secondary).opacity(0.35) }
            HStack(alignment: .top, spacing: 6) {
                if let hint = window.hint, !hint.isEmpty { Text(hint).lineLimit(2).fixedSize(horizontal: false, vertical: true) }
                else if let text = window.text, !text.isEmpty { Text(text).lineLimit(2).fixedSize(horizontal: false, vertical: true) }
                else { Text("Ritmo indisponível") }
                Spacer(minLength: 4)
                if let reset = window.reset, reset != "-" {
                    Label(reset, systemImage: "arrow.counterclockwise").font(.system(size: 9, weight: .medium)).lineLimit(1).fixedSize()
                } else if let countdown = DisplayFormat.resetCountdown(until: window.resetsAt) {
                    Label(countdown, systemImage: "arrow.counterclockwise").font(.system(size: 9, weight: .medium)).lineLimit(1).fixedSize()
                }
            }.font(.system(size: 9)).foregroundStyle(.secondary)
        }
    }

    private func quotaColor(_ color: String?) -> Color {
        switch color { case "red": .red; case "orange": .orange; case "green": .green; default: .secondary }
    }

    private func detailRow(_ title: String, value: String, symbol: String) -> some View {
        HStack(spacing: 8) {
            Image(systemName: symbol).font(.system(size: 11)).foregroundStyle(.secondary).frame(width: 17)
            Text(title).font(.system(size: 10)).foregroundStyle(.secondary)
            Spacer()
            Text(value).font(.system(size: 10, weight: .medium)).multilineTextAlignment(.trailing)
        }
    }

    private func statusPill(_ title: String, icon: String, color: Color) -> some View {
        Label(title, systemImage: icon).font(.system(size: 9, weight: .medium)).foregroundStyle(color)
            .padding(.horizontal, 9).padding(.vertical, 5).background(color.opacity(0.1), in: Capsule())
    }

    private func errorBanner(_ message: String) -> some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: "exclamationmark.triangle.fill").font(.system(size: 11)).foregroundStyle(.orange)
            Text(message).font(.system(size: 10)).foregroundStyle(.primary.opacity(0.8)).fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 0)
        }.padding(10).frame(maxWidth: .infinity, alignment: .leading).background(Color.orange.opacity(0.09), in: RoundedRectangle(cornerRadius: 10))
    }

    private var actions: some View {
        VStack(spacing: 8) {
            Button {
                guard !model.isFixture else { return }
                let executable = URL(fileURLWithPath: NSHomeDirectory()).appending(path: ".local/bin/costs")
                let process = Process(); process.executableURL = executable
                process.arguments = ["report", "--format", "html", "--open"]
                do { try process.run() }
                catch { model.errorMessage = "Não foi possível abrir o relatório: \(error.localizedDescription)" }
            } label: {
                Label("Abrir relatório completo", systemImage: "chart.bar.xaxis").font(.system(size: 11, weight: .medium))
                    .frame(maxWidth: .infinity).padding(.vertical, 9).background(Color.accentColor.opacity(0.1), in: RoundedRectangle(cornerRadius: 10))
            }.buttonStyle(.plain).disabled(model.isFixture)
                .accessibilityIdentifier("open-report")
            HStack {
                Text(model.isFixture ? "DEMONSTRAÇÃO · somente leitura" : "Somente cache · sem atualização de cota")
                Spacer()
            }.font(.system(size: 8, weight: .medium)).tracking(0.3).foregroundStyle(.tertiary)
        }.padding(.top, 1)
    }

    private var footer: some View {
        HStack(spacing: 5) {
            Image(systemName: "desktopcomputer").font(.system(size: 8))
            Text("Custos sincronizados · \(model.snapshot?.syncAge ?? "idade indisponível")")
            Spacer()
            if model.snapshot?.stale == true { Text("SINCRONIZAÇÃO ANTIGA").foregroundStyle(.orange) }
        }.font(.system(size: 8, weight: .medium)).tracking(0.2).foregroundStyle(.secondary)
            .padding(.horizontal, 6).padding(.vertical, 6).background(Color.primary.opacity(0.025))
            .overlay(alignment: .top) { Divider().opacity(0.5) }
    }
}
