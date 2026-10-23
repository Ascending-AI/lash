/*
 * Javascript Micro benchmark
 *
 * Copyright (c) 2017-2019 Fabrice Bellard
 * Copyright (c) 2017-2019 Charlie Gordon
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in
 * all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL
 * THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
 * THE SOFTWARE.
 */
function benchmark(trips) {
var global_res=0;
var seed=7;
function random_value() { seed=(seed*16807)%2147483647; return seed/2147483647; }


    function random(arr, n, def) {
        for (var i = 0; i < n; i++)
            arr[i] = def[(random_value() * n) >> 0];
    }
    function random8(arr, n, def) {
        for (var i = 0; i < n; i++)
            arr[i] = def[(random_value() * 256) >> 0];
    }
    function random1(arr, n, def) {
        for (var i = 0; i < n; i++)
            arr[i] = def[(random_value() * 2) >> 0];
    }
    function hill(arr, n, def) {
        var mid = n >> 1;
        for (var i = 0; i < mid; i++)
            arr[i] = def[i];
        for (var i = mid; i < n; i++)
            arr[i] = def[n - i];
    }
    function comb(arr, n, def) {
        for (var i = 0; i < n; i++)
            arr[i] = def[(i & 1) * i];
    }
    function crisscross(arr, n, def) {
        for (var i = 0; i < n; i++)
            arr[i] = def[(i & 1) ? n - i : i];
    }
    function zero(arr, n, def) {
        for (var i = 0; i < n; i++)
            arr[i] = def[0];
    }
    function increasing(arr, n, def) {
        for (var i = 0; i < n; i++)
            arr[i] = def[i];
    }
    function decreasing(arr, n, def) {
        for (var i = 0; i < n; i++)
            arr[i] = def[n - 1 - i];
    }
    function alternate(arr, n, def) {
        for (var i = 0; i < n; i++)
            arr[i] = def[i ^ 1];
    }
    function jigsaw(arr, n, def) {
        for (var i = 0; i < n; i++)
            arr[i] = def[i % (n >> 4)];
    }
    function incbutone(arr, n, def) {
        for (var i = 0; i < n; i++)
            arr[i] = def[i];
        if (n > 0)
            arr[n >> 2] = def[n];
    }
    function incbutfirst(arr, n, def) {
        if (n > 0)
            arr[0] = def[n];
        for (var i = 1; i < n; i++)
            arr[i] = def[i];
    }
    function incbutlast(arr, n, def) {
        for (var i = 0; i < n - 1; i++)
            arr[i] = def[i + 1];
        if (n > 0)
            arr[n - 1] = def[0];
    }

    var sort_cases = [ random, random8, random1, jigsaw, hill, comb,
                      crisscross, zero, increasing, decreasing, alternate,
                      incbutone, incbutlast, incbutfirst ];


for (var trip=0;trip<trips;trip++) {
var n=1000; var def=[]; for(var i=0;i<=n;i++) def[i]=i+""; def.sort();
for(var case_index=0;case_index<sort_cases.length;case_index++){var arr=[]; sort_cases[case_index](arr,n,def); arr.sort(); for(var i=1;i<n;i++){if(arr[i]!==undefined && arr[i-1]>arr[i]) throw Error("sort order");} global_res += arr.length;}
}
return global_res;
}
