/** Languages supported by ElevenLabs Scribe, keyed by ISO-639-3 code. */
export const LANGUAGES = [
  ["afr", "Afrikaans"],
  ["amh", "Amharic"],
  ["ara", "Arabic"],
  ["hye", "Armenian"],
  ["asm", "Assamese"],
  ["ast", "Asturian"],
  ["aze", "Azerbaijani"],
  ["bel", "Belarusian"],
  ["ben", "Bengali"],
  ["bos", "Bosnian"],
  ["bul", "Bulgarian"],
  ["mya", "Burmese"],
  ["yue", "Cantonese"],
  ["cat", "Catalan"],
  ["ceb", "Cebuano"],
  ["nya", "Chichewa"],
  ["hrv", "Croatian"],
  ["ces", "Czech"],
  ["dan", "Danish"],
  ["nld", "Dutch"],
  ["eng", "English"],
  ["est", "Estonian"],
  ["fil", "Filipino"],
  ["fin", "Finnish"],
  ["fra", "French"],
  ["ful", "Fulah"],
  ["glg", "Galician"],
  ["lug", "Ganda"],
  ["kat", "Georgian"],
  ["deu", "German"],
  ["ell", "Greek"],
  ["guj", "Gujarati"],
  ["hau", "Hausa"],
  ["heb", "Hebrew"],
  ["hin", "Hindi"],
  ["hun", "Hungarian"],
  ["isl", "Icelandic"],
  ["ibo", "Igbo"],
  ["ind", "Indonesian"],
  ["gle", "Irish"],
  ["ita", "Italian"],
  ["jpn", "Japanese"],
  ["jav", "Javanese"],
  ["kea", "Kabuverdianu"],
  ["kan", "Kannada"],
  ["kaz", "Kazakh"],
  ["khm", "Khmer"],
  ["kor", "Korean"],
  ["kur", "Kurdish"],
  ["kir", "Kyrgyz"],
  ["lao", "Lao"],
  ["lav", "Latvian"],
  ["lin", "Lingala"],
  ["lit", "Lithuanian"],
  ["luo", "Luo"],
  ["ltz", "Luxembourgish"],
  ["mkd", "Macedonian"],
  ["msa", "Malay"],
  ["mal", "Malayalam"],
  ["mlt", "Maltese"],
  ["cmn", "Mandarin Chinese"],
  ["mri", "Māori"],
  ["mar", "Marathi"],
  ["mon", "Mongolian"],
  ["nep", "Nepali"],
  ["nso", "Northern Sotho"],
  ["nor", "Norwegian"],
  ["oci", "Occitan"],
  ["ori", "Odia"],
  ["pus", "Pashto"],
  ["fas", "Persian"],
  ["pol", "Polish"],
  ["por", "Portuguese"],
  ["pan", "Punjabi"],
  ["ron", "Romanian"],
  ["rus", "Russian"],
  ["srp", "Serbian"],
  ["sna", "Shona"],
  ["snd", "Sindhi"],
  ["slk", "Slovak"],
  ["slv", "Slovenian"],
  ["som", "Somali"],
  ["spa", "Spanish"],
  ["swa", "Swahili"],
  ["swe", "Swedish"],
  ["tgk", "Tajik"],
  ["tam", "Tamil"],
  ["tel", "Telugu"],
  ["tha", "Thai"],
  ["tur", "Turkish"],
  ["ukr", "Ukrainian"],
  ["umb", "Umbundu"],
  ["urd", "Urdu"],
  ["uzb", "Uzbek"],
  ["vie", "Vietnamese"],
  ["cym", "Welsh"],
  ["wol", "Wolof"],
  ["xho", "Xhosa"],
  ["zul", "Zulu"],
];

const NAMES = new Map(LANGUAGES);

export function languageName(code) {
  return NAMES.get(code) ?? code;
}

/** Lowercase without accents, so "maori" matches "Māori". */
function fold(text) {
  return text.normalize("NFD").replace(/\p{Diacritic}/gu, "").toLowerCase();
}

function attempt(read) {
  try {
    return read();
  } catch {
    return null;
  }
}

let index = null;

// Built on first search: the web view knows each language's own name and two-letter code.
function searchIndex() {
  index ??= LANGUAGES.map(([code, name]) => {
    const short = attempt(() => Intl.getCanonicalLocales(code)[0]);
    const native = attempt(() => new Intl.DisplayNames([code], { type: "language" }).of(code));
    return {
      code,
      name,
      codes: [code, short].filter(Boolean).map(fold),
      names: [name, native].filter((text) => text && text !== code).map(fold),
    };
  });
  return index;
}

function rank(entry, query) {
  if (entry.codes.includes(query)) return 0;
  if (entry.names.some((name) => name.startsWith(query))) return 1;
  if (entry.names.some((name) => name.split(/[\s-]+/).some((word) => word.startsWith(query)))) return 2;
  if (entry.names.some((name) => name.includes(query))) return 3;
  return null;
}

/** Languages matching `query` by name, own name or code, best matches first. */
export function searchLanguages(query, exclude = []) {
  const skip = new Set(exclude);
  const folded = fold(query.trim());
  const matches = [];
  for (const entry of searchIndex()) {
    if (skip.has(entry.code)) continue;
    const score = folded ? rank(entry, folded) : 0;
    if (score !== null) matches.push({ entry, score });
  }
  // Array sort is stable, so each tier stays alphabetical.
  return matches.sort((a, b) => a.score - b.score).map(({ entry }) => [entry.code, entry.name]);
}
