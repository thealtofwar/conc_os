#include "textflag.h"

// func vmmcall(nr uint64) uint64
TEXT ·vmmcall(SB), NOSPLIT, $0-16
	MOVQ nr+0(FP), AX
	BYTE $0x0F; BYTE $0x01; BYTE $0xD9 // VMMCALL
	MOVQ AX, ret+8(FP)
	RET
