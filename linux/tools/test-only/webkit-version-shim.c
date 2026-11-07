/* TEST ONLY, for the headless rig while the system WebKitGTK is older than
 * Zephium's security floor: reports 2.54.0 so startup can be exercised.
 * Never use it to browse; update webkit2gtk-4.1 instead. */
unsigned int webkit_get_major_version(void) { return 2; }
unsigned int webkit_get_minor_version(void) { return 54; }
unsigned int webkit_get_micro_version(void) { return 0; }
