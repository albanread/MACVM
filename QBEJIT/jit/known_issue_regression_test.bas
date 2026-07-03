REM Minimal reproduction of the nested WHILE bug with array assignment
DIM sieve(100) AS INTEGER
DIM i AS INTEGER
DIM j AS INTEGER

REM Initialize
i = 1
WHILE i <= 10
    sieve(i) = 1
    i = i + 1
WEND

PRINT "Starting nested loop test"
i = 2
j = i * i
PRINT "i = "; i; ", j = "; j

WHILE j <= 10
    PRINT "Marking sieve("; j; ") = 0"
    sieve(j) = 0
    PRINT "After: sieve("; j; ") = "; sieve(j)
    j = j + i
    PRINT "Next j = "; j
WEND

PRINT "Loop done"
PRINT "sieve(4) = "; sieve(4)
PRINT "sieve(6) = "; sieve(6)
PRINT "sieve(8) = "; sieve(8)
