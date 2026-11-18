'use client'

import { useEffect } from 'react'
import { ThemeProvider as NextThemesProvider, useTheme } from 'next-themes'
import { isTauri } from '@tauri-apps/api/core'
import { Toaster } from 'sonner'

export type ThemePreference = 'system' | 'light' | 'dark'

export const THEME_STORAGE_KEY = 'meetily-theme'

/**
 * Keeps the native window chrome (title bar, and the webview's
 * prefers-color-scheme) in step with the in-app preference. `null` hands the
 * window back to the OS, which is what makes "System" follow live OS changes.
 */
function NativeWindowThemeSync() {
  const { theme } = useTheme()

  useEffect(() => {
    if (!theme || !isTauri()) return
    const native = theme === 'light' || theme === 'dark' ? theme : null
    import('@tauri-apps/api/window')
      .then(({ getCurrentWindow }) => getCurrentWindow().setTheme(native))
      .catch((error) => console.warn('[Theme] Failed to set native window theme:', error))
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
