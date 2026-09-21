// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 架构识别窗口测试程序
//
//   文件:       tests/fixtures/architecture_window.cpp
//
//   日期:       2026年09月21日
//   环境:       Windows Build 29671.1000 ARM64；MSVC 14.51.36231
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

// A disposable GUI process compiled independently for each architecture. Tests wait for its
// input queue to become idle, then inspect the real process through the production sampler.
#include <windows.h>

#if defined(_M_ARM64EC)
constexpr wchar_t title[] = L"Architecture fixture - ARM64EC";
#elif defined(_M_ARM64)
constexpr wchar_t title[] = L"Architecture fixture - ARM64";
#elif defined(_M_X64)
constexpr wchar_t title[] = L"Architecture fixture - x86-64";
#else
constexpr wchar_t title[] = L"Architecture fixture - x86";
#endif

LRESULT CALLBACK window_proc(HWND window, UINT message, WPARAM wp, LPARAM lp) {
    if (message == WM_DESTROY) {
        PostQuitMessage(0);
        return 0;
    }
    return DefWindowProcW(window, message, wp, lp);
}

int WINAPI wWinMain(HINSTANCE instance, HINSTANCE, PWSTR, int show) {
    WNDCLASSW cls{};
    cls.lpfnWndProc = window_proc;
    cls.hInstance = instance;
    cls.lpszClassName = L"TaskmgrArchitectureFixture";
    cls.hbrBackground = reinterpret_cast<HBRUSH>(COLOR_WINDOW + 1);
    if (!RegisterClassW(&cls)) return 1;
    HWND window = CreateWindowW(cls.lpszClassName, title, WS_OVERLAPPEDWINDOW,
        CW_USEDEFAULT, CW_USEDEFAULT, 420, 160, nullptr, nullptr, instance, nullptr);
    if (!window) return 2;
    ShowWindow(window, show);
    MSG message{};
    BOOL result;
    while ((result = GetMessageW(&message, nullptr, 0, 0)) > 0) {
        TranslateMessage(&message);
        DispatchMessageW(&message);
    }
    return result == -1 ? 3 : 0;
}
