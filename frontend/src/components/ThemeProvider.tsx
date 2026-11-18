'use client'

import { useEffect } from 'react'
import { ThemeProvider as NextThemesProvider, useTheme } from 'next-themes'
import { isTauri } from '@tauri-apps/api/core'
import { Toaster } from 'sonner'

export type ThemePreference = 'system' | 'light' | 'dark'

export const THEME_STORAGE_KEY = 'meetily-theme'

/**
 * Hands the preference to the native window chrome (title bar, and the
 * webview's prefers-color-scheme). `null` gives the window back to the OS,
 * which is what makes "System" follow live OS changes.
 */
function syncNativeWindowTheme(preference: string | null | undefined) {
  if (!isTauri()) return
  const native = preference === 'light' || preference === 'dark' ? preference : null
  import('@tauri-apps/api/window')
    .then(({ getCurrentWindow }) => getCurrentWindow().setTheme(native))
    .catch((error) => console.warn('[Theme] Failed to set native window theme:', error))
}

// Apply the stored preference as soon as this bundle loads rather than after
// React mounts, so a saved Light/Dark that differs from the OS leaves the
// title bar mismatched only until the JS bundle has loaded. The window opens
// with the OS theme because tauri.conf.json no longer forces one.
if (typeof window !== 'undefined') {
  try {
    syncNativeWindowTheme(window.localStorage.getItem(THEME_STORAGE_KEY))
  } catch {
    // localStorage unavailable: the effect below syncs after mount instead.
  }
}

function NativeWindowThemeSync() {
  const { theme } = useTheme()

  useEffect(() => {
    if (theme) syncNativeWindowTheme(theme)
  }, [theme])

  return null
}

export function ThemeProvider({ children }: { children: React.ReactNode }) {
  return (
    <NextThemesProvider
      attribute="class"
      defaultTheme="system"
      enableSystem
      storageKey={THEME_STORAGE_KEY}
      disableTransitionOnChange
    >
      <NativeWindowThemeSync />
      {children}
    </NextThemesProvider>
  )
}

export function ThemedToaster() {
  const { resolvedTheme } = useTheme()
  return (
    <Toaster
      position="bottom-center"
      richColors
      closeButton
      theme={resolvedTheme === 'dark' ? 'dark' : 'light'}
    />
  )
}

/** The effective theme for components that take an explicit light/dark prop. */
export function useResolvedTheme(): 'light' | 'dark' {
  const { resolvedTheme } = useTheme()
  return resolvedTheme === 'dark' ? 'dark' : 'light'
}
