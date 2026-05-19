/// <reference types="@raycast/api">

/* 🚧 🚧 🚧
 * This file is auto-generated from the extension's manifest.
 * Do not modify manually. Instead, update the `package.json` file.
 * 🚧 🚧 🚧 */

/* eslint-disable @typescript-eslint/ban-types */

type ExtensionPreferences = {
  /** Findr Binary Path - Path to the findr binary */
  "findrPath": string,
  /** Max Results - Maximum number of search results */
  "maxResults": string
}

/** Preferences accessible in all the extension's commands */
declare type Preferences = ExtensionPreferences

declare namespace Preferences {
  /** Preferences accessible in the `search` command */
  export type Search = ExtensionPreferences & {}
  /** Preferences accessible in the `search-content` command */
  export type SearchContent = ExtensionPreferences & {}
  /** Preferences accessible in the `reindex` command */
  export type Reindex = ExtensionPreferences & {}
}

declare namespace Arguments {
  /** Arguments passed to the `search` command */
  export type Search = {}
  /** Arguments passed to the `search-content` command */
  export type SearchContent = {}
  /** Arguments passed to the `reindex` command */
  export type Reindex = {}
}

