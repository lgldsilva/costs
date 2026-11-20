import ServiceManagement

struct MainAppLoginService: LoginServiceControlling {
    func register() throws { try SMAppService.mainApp.register() }
    func unregister() throws { try SMAppService.mainApp.unregister() }
}
