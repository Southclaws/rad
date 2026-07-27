import { defineI18n } from "fumadocs-core/i18n";

// English keeps its existing /docs URLs. New locales receive a prefix, such as
// /fr/docs. A locale only exposes documents that have been written for it.
export const i18n = defineI18n({
  defaultLanguage: "en",
  languages: ["en", "fr"],
  hideLocale: "default-locale",
  parser: "dot",
  fallbackLanguage: null,
});

export type DocsLocale = (typeof i18n.languages)[number];

export function isDocsLocale(locale: string): locale is DocsLocale {
  return i18n.languages.includes(locale as DocsLocale);
}
