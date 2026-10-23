let log = ''; function mark(x: number): number { log += 'x'; return x; } enum E { A = mark(2), B = mark(3) } [log, E.A, E.B];
