/** @type {import('tailwindcss').Config} */
export default {
  darkMode: 'class',
  content: ['./index.html', './src/**/*.{js,ts,jsx,tsx}'],
  theme: {
    extend: {
      colors: {
        deepseek: {
          bg: 'var(--bg-deepseek-main)',
          sidebar: 'var(--bg-deepseek-sidebar)',
          border: 'var(--border-deepseek)',
          text: 'var(--text-deepseek-main)',
          textSecondary: 'var(--text-deepseek-secondary)',
          accent: 'var(--bg-deepseek-accent)',
          hover: 'var(--bg-deepseek-hover)',
          btnHover: 'var(--bg-deepseek-btn-hover)',
          // Components write the kebab-case class (bg-deepseek-btn-hover).
          'btn-hover': 'var(--bg-deepseek-btn-hover)',
          input: 'var(--bg-deepseek-input)',
          avatar: 'var(--bg-deepseek-avatar)',
          avatarText: 'var(--text-deepseek-avatar)',
          tooltipBg: 'var(--bg-tooltip)',
          tooltipText: 'var(--text-tooltip)',
          navActive: 'var(--text-nav-active)',
          navDisabled: 'var(--text-nav-disabled)',
        }
      },
      fontFamily: {
        sans: ['Anthropic Sans', 'Figtree Variable', 'Figtree', 'system-ui', 'Microsoft YaHei', 'sans-serif'],
        serif: ['Anthropic Serif', 'Source Serif 4 Variable', 'Source Serif 4', 'serif'],
        system: ['system-ui', '-apple-system', 'BlinkMacSystemFont', '"Segoe UI"', 'Roboto', 'Oxygen', 'Ubuntu', 'Cantarell', '"Open Sans"', '"Helvetica Neue"', 'sans-serif'],
      }
    }
  },
  plugins: [],
}
