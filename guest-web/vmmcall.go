package main

// vmmcall executes the VMMCALL instruction with nr in RAX and returns RAX.
// conc_os intercepts it from any privilege level; unknown numbers return
// all-ones.
//
//go:noescape
func vmmcall(nr uint64) uint64
